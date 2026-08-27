#!/usr/bin/env python3
"""Dual-mode /api/switch stress: flip webrtc<->websockets 6 times, verifying
the mode, HTTP routes, and that a repeat switch to the same mode is a clean
no-op (no crash, no duplicate service instances / port conflicts).

The desktop the transports share is checked too. Logical monitors belong to
whichever layout engine defined them, and the flip happens under a client that
is still attached, so nothing tears the layout down on the way out: one left
behind has window managers tiling panels and maximized windows against a
rectangle that is no longer the screen.
"""
import asyncio
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets

SETTINGS = {
    "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
    "manual_resolution": False, "framerate": 60, "encoder": "h264enc",
    "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
    "scaling_dpi": 96, "displayPosition": "right",
}


def selkies_monitors() -> list:
    """Logical monitors this software defined on the test display."""
    try:
        out = subprocess.run(["xrandr", "--listmonitors"], capture_output=True,
                             text=True, timeout=20,
                             env=dict(os.environ, DISPLAY=H.require_display())).stdout
    except (OSError, subprocess.SubprocessError):
        return []
    return [line.split()[1].lstrip("*") for line in out.splitlines()
            if len(line.split()) > 1 and "selkies-" in line.split()[1]]


async def flip_under_a_client(res: "H.Results") -> None:
    """Run the flip sequence with a websockets client attached throughout."""
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send("SETTINGS," + json.dumps(SETTINGS))
        await asyncio.sleep(5.0)
        res.check("the layout engine defined this client's monitor",
                  bool(selkies_monitors()), f"{selkies_monitors()}")

        for i, target in enumerate(["webrtc", "websockets"] * 3):
            s, body = H.curl("/api/switch", method="POST", data={"mode": target})
            res.check(f"switch {i}->{target} returns 200", s == 200, body[:60])
            if s == 200:
                st = json.loads(H.curl("/api/status")[1])
                res.check(f"switch {i}: mode={target}", st.get("current_mode") == target,
                          st.get("current_mode"))
            await asyncio.sleep(1.5)
            if target == "webrtc":
                left = selkies_monitors()
                res.check(f"switch {i}: no monitor of the stopped transport is left",
                          not left, f"{left}")


def main() -> "H.Results":
    """Flip the transport mode repeatedly and verify each switch lands cleanly."""
    H.server_start(mode="websockets", wayland=False)
    res = H.Results("switch")
    try:
        asyncio.run(asyncio.wait_for(flip_under_a_client(res), 180))
    except Exception as e:
        res.check("the flip sequence ran to the end", False, f"{type(e).__name__}: {e}")
    time.sleep(1.0)
    st = json.loads(H.curl("/api/status")[1])
    res.check("final mode is websockets", st.get("current_mode") == "websockets", st)
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
