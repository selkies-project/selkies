#!/usr/bin/env python3
"""What the remote pointer does with a relative motion message.

The client sends locked and trackpad motion as deltas, and both backends inject
them verbatim: no acceleration curve of their own, no loss when a movement
arrives split into many small deltas rather than one large one. That is what
makes the client's own scaling the only thing standing between a hand's
movement and the remote pointer's, and it has to hold identically on X11 (XTEST
relative motion) and on Wayland (the compositor's relative pointer), or the
same gesture lands differently on the two backends.

The seam back to absolute mode is pinned too: a drag held against the screen
edge stops at the edge and unwinds from there rather than from where the deltas
would have carried it, and the absolute move after it lands where it says,
whatever the tracked position made of the drag.
"""
import asyncio
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets

WL_SOCKET = "wayland-1"
START = (600, 400)
TRAVEL = 120


def settings_payload() -> dict:
    """A primary-display SETTINGS payload, enough to bring the session up."""
    return {
        "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
        "is_manual_resolution_mode": False, "framerate": 60, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    }


async def send_all(messages, settle: float = 0.35) -> None:
    """Open one client socket, send each message in order, and let it land."""
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send("SETTINGS," + json.dumps(settings_payload()))
        await asyncio.sleep(2.0)
        for message in messages:
            await ws.send(message)
            await asyncio.sleep(0.01)
        await asyncio.sleep(settle)


def x11_block(res) -> None:
    """Relative motion against the X test display."""
    H.server_start(mode="websockets", wayland=False)
    d = H.x_display()
    try:
        root = d.screen().root

        def pointer_x() -> int:
            return root.query_pointer().root_x

        def travel(messages) -> int:
            asyncio.run(send_all([f"m,{START[0]},{START[1]},0,0"] + list(messages)))
            return pointer_x() - START[0]

        one = travel([f"m2,{TRAVEL},0,0,0"])
        res.check("x11: one delta moves the pointer by exactly that much",
                  one == TRAVEL, f"{one}")
        many = travel(["m2,1,0,0,0"] * TRAVEL)
        res.check("x11: the same movement split into single pixels travels as far",
                  many == TRAVEL, f"{many}")
        back = travel([f"m2,{TRAVEL},0,0,0", f"m2,{-TRAVEL},0,0,0"])
        res.check("x11: there and back leaves the pointer where it started",
                  back == 0, f"{back}")
        asyncio.run(send_all(["m,600,400,0,0", "m2,-700,0,0,0", "m2,300,0,0,0"]))
        unwound = pointer_x()
        res.check("x11: a drag pinned against the edge unwinds from the edge",
                  unwound == 300, f"{unwound}")
        asyncio.run(send_all(["m,600,400,0,0", "m2,-700,0,0,0",
                              "m2,300,0,0,0", "m,200,400,0,0"]))
        pinned = pointer_x()
        res.check("x11: an absolute move after the drag lands where it says",
                  pinned == 200, f"{pinned}")
    finally:
        d.close()
        H.server_stop()


def wayland_block(res) -> None:
    """The same motion against the Wayland backend's seat."""
    try:
        from pixelflux import ScreenCapture
        rung = ("relative injection" if hasattr(ScreenCapture, "inject_relative_mouse_move")
                else "the absolute fallback")
    except ImportError:
        rung = "no pixelflux at all"
    print(f"[relative-injection] PREFLIGHT: wayland deltas go through {rung}", flush=True)
    H.server_start(mode="websockets", wayland=True)
    obs = H.WlObs(WL_SOCKET)
    try:
        if not obs.ready(20):
            res.skip("wayland: the observer surface maps", "no mapped event")
            return

        def travel(messages) -> float:
            obs.lines.clear()
            asyncio.run(send_all([f"m,{START[0]},{START[1]},0,0"] + list(messages)))
            time.sleep(0.5)
            # The pointer's first arrival on the surface is an enter, not a
            # motion, and it carries the position the opening absolute message
            # put it at: the origin to measure the deltas from.
            seen = [line["x"] for line in obs.lines
                    if line.get("kind") in ("ptr_enter", "ptr_motion")]
            return seen[-1] - seen[0] if len(seen) >= 2 else float("nan")

        one = travel([f"m2,{TRAVEL},0,0,0"])
        res.check("wayland: one delta moves the pointer by exactly that much",
                  one == TRAVEL, f"{one}")
        many = travel(["m2,1,0,0,0"] * TRAVEL)
        res.check("wayland: the same movement split into single pixels travels as far",
                  many == TRAVEL, f"{many}")
        back = travel([f"m2,{TRAVEL},0,0,0", f"m2,{-TRAVEL},0,0,0"])
        res.check("wayland: there and back leaves the pointer where it started",
                  back == 0, f"{back}")
        def last_seen() -> float:
            seen = [line["x"] for line in obs.lines
                    if line.get("kind") in ("ptr_enter", "ptr_motion")]
            return seen[-1] if seen else float("nan")

        obs.lines.clear()
        asyncio.run(send_all(["m,600,400,0,0", "m2,-700,0,0,0", "m2,300,0,0,0"]))
        time.sleep(0.5)
        unwound = last_seen()
        res.check("wayland: a drag pinned against the edge unwinds from the edge",
                  unwound == 300, f"{unwound}")
        obs.lines.clear()
        asyncio.run(send_all(["m,600,400,0,0", "m2,-700,0,0,0",
                              "m2,300,0,0,0", "m,200,400,0,0"]))
        time.sleep(0.5)
        pinned = last_seen()
        res.check("wayland: an absolute move after the drag lands where it says",
                  pinned == 200, f"{pinned}")
    finally:
        obs.stop()
        H.server_stop()


def run() -> "H.Results":
    res = H.Results("relative-injection")
    x11_block(res)
    wayland_block(res)
    res.summary()
    return res


if __name__ == "__main__":
    r = run()
    sys.exit(0 if not r.failed() else 1)
