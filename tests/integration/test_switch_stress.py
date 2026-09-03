#!/usr/bin/env python3
"""Dual-mode /api/switch stress: flip webrtc<->websockets 6 times, verifying
the mode, HTTP routes, and that a repeat switch to the same mode is a clean
no-op (no crash, no duplicate service instances / port conflicts).

The desktop the transports share is checked too. Logical monitors belong to
whichever layout engine defined them, and the flip happens under a client that
is still attached, so nothing tears the layout down on the way out: one left
behind has window managers tiling panels and maximized windows against a
rectangle that is no longer the screen. That attached client is also what makes
the capture teardown worth checking here: its display entry outlives the switch
by the reconnect grace, so the reconfigure pass takes its clients-present branch
and a shutdown that leaned on it would leave the capture encoding for nobody.
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

# Set from argv at entry; the monitor checks are X11's, the teardown checks are not.
IS_WAYLAND = False

SETTINGS = {
    "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
    "manual_resolution": False, "framerate": 60, "encoder": "h264enc",
    "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
    "scaling_dpi": 96, "displayPosition": "right",
}


def shutdown_windows() -> list:
    """The log between each `DataStreamingServer` shutdown's first and last line."""
    log = H.server_log()
    windows = []
    at = 0
    while True:
        start = log.find("DataStreamingServer shutdown initiated", at)
        if start < 0:
            return windows
        end = log.find("DataStreamingServer shutdown complete", start)
        if end < 0:
            return windows
        windows.append(log[start:end])
        at = end


def selkies_monitors() -> list:
    """Logical monitors this software defined on the test display; none on
    Wayland, where the layout engine owns compositor outputs instead."""
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
        if not IS_WAYLAND:
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
            if target == "webrtc" and not IS_WAYLAND:
                left = selkies_monitors()
                res.check(f"switch {i}: no monitor of the stopped transport is left",
                          not left, f"{left}")

    # A shutdown whose reconfigure pass still saw display clients (the reconnect
    # grace holds them well past the switch) is one where that pass stopped
    # nothing, so the capture is shutdown's own to stop.
    held = [w for w in shutdown_windows()
            if "Calculating new extended desktop layout" in w]
    res.check("a shutdown that outran the reconnect grace stopped its own capture",
              held and all("Stopping all streams" in w for w in held),
              f"{len(held)} of {len(shutdown_windows())} shutdown(s) held clients")
    res.check("no cursor callback outlived its server",
              "Error handling pixelflux cursor" not in H.server_log())


def main(wayland: bool = False) -> "H.Results":
    """Flip the transport mode repeatedly and verify each switch lands cleanly.

    Args:
        wayland: Run the flip against the Wayland backend instead of X11. The
            capture teardown is the same code either way, but only one of them
            has a compositor still reporting cursors into it.
    """
    H.server_start(mode="websockets", wayland=wayland)
    res = H.Results("switch-wayland" if wayland else "switch")
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
    IS_WAYLAND = "wayland" in sys.argv[1:]
    r = main(IS_WAYLAND)
    sys.exit(0 if not r.failed() else 1)
