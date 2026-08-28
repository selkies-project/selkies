#!/usr/bin/env python3
"""The latency the server reports is the path's, not the client's ack cadence.

Frame acks carry how long the client held the id before its ack tick fired.
A backgrounded tab clamps that tick to a second, so without the hold the
server would read a second of the client's own timer as network latency and
report it for as long as the smoothing window holds those samples -- the
"latency sits near 1000 ms for ten seconds after coming back to the tab"
report. Held acks are replayed here with and without the hold, over the real
websocket, and the reported figure has to follow the path either way.

Usage: python3 tests/integration/test_ack_latency.py
"""
import asyncio
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

try:
    import websockets
except ImportError:
    H.skip_suite("websockets is not installed")

SETTINGS = {
    "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
    "manual_resolution": False, "framerate": 60, "encoder": "h264enc",
    "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
    "scaling_dpi": 96, "displayPosition": "right",
}
# One round per smoothing sample, so the window is entirely held acks.
ROUNDS = 22
HOLD_S = 0.3
HOLD_MS = int(HOLD_S * 1000)


async def _read_until(ws, deadline: float, state: dict) -> None:
    """Drain the socket until `deadline`, tracking the newest frame id and the
    last reported latency."""
    while time.monotonic() < deadline:
        try:
            m = await asyncio.wait_for(ws.recv(), timeout=0.2)
        except asyncio.TimeoutError:
            continue
        if isinstance(m, (bytes, bytearray)):
            if len(m) >= 4 and m[0] in (0x03, 0x04):
                fid = (m[2] << 8) | m[3]
                if fid != state.get("id"):
                    state["id"] = fid
                    state["id_at"] = time.monotonic()
        elif isinstance(m, str) and "latency_ms" in m:
            try:
                v = json.loads(m).get("latency_ms")
            except ValueError:
                v = None
            if v is not None:
                state["latency"] = v


async def _ack(ws, state: dict, report_hold: bool) -> None:
    """Ack the newest id, reporting how long it really waited when asked to."""
    fid = state.get("id")
    if fid is None or fid == state.get("acked"):
        return
    state["acked"] = fid
    held = int(max(0.0, (time.monotonic() - state.get("id_at", 0)) * 1000))
    await ws.send(f"CLIENT_FRAME_ACK {fid} {held}" if report_hold
                  else f"CLIENT_FRAME_ACK {fid}")


async def held_acks(ws, state: dict, report_hold: bool) -> float:
    """Fill the smoothing window with acks held `HOLD_S` past their frame; the
    latency the server reports afterwards. The socket is left unread through
    the hold, so the id really is that old when its ack goes out -- a client
    whose ack tick was clamped, not one that simply chose a stale id."""
    for _ in range(ROUNDS):
        await _read_until(ws, time.monotonic() + 0.1, state)
        await asyncio.sleep(HOLD_S)
        await _ack(ws, state, report_hold)
    await _read_until(ws, time.monotonic() + 3.0, state)
    return state.get("latency", -1.0)


async def drive(res: "H.Results") -> None:
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send("SETTINGS," + json.dumps(SETTINGS))
        state = {}
        # Prompt acks first, so the window starts on real round trips.
        end = time.monotonic() + 10
        while time.monotonic() < end:
            await _read_until(ws, time.monotonic() + 0.05, state)
            await _ack(ws, state, True)
        prompt = state.get("latency", -1.0)
        res.check("prompt acks report the path", 0 <= prompt < HOLD_MS / 2, prompt)

        reported = await held_acks(ws, state, report_hold=True)
        res.check("a held ack that reports its hold still reports the path",
                  0 <= reported < HOLD_MS / 2, reported)

        # The control: the same holds, unreported, do move the figure -- so the
        # check above is measuring the subtraction and not a quiet link.
        silent = await held_acks(ws, state, report_hold=False)
        res.check("an ack that hides its hold is taken at face value",
                  silent > HOLD_MS / 2, silent)


def run() -> "H.Results":
    res = H.Results("ack-latency")
    H.server_start(mode="websockets", extra_env={"SELKIES_USE_CPU": "true"})
    try:
        asyncio.run(drive(res))
    finally:
        H.server_stop()
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
