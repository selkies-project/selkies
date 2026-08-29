#!/usr/bin/env python3
"""Where a client's cursor shapes come from on X11.

Two sources feed one client message. A connecting client is seeded straight
from XFixes on the loop (``get_current_cursor_data``), because pixelflux only
delivers a cursor while something is capturing; every later shape arrives from
pixelflux's own XFixes thread through ``set_cursor_callback``. There is no
python cursor loop between them, so this pins that the seed lands, that a
change made while streaming reaches the client, and that a change made while
nothing captures is not lost -- the capture restart re-fetches it.

Usage: python3 tests/integration/test_cursor_delivery.py
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

# Two X font-cursor glyphs whose sprites differ, so a delivered PNG identifies
# which one the server read.
GLYPH_A = 68    # XC_hand2
GLYPH_B = 150   # XC_spraycan

SETTINGS = {
    "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
    "manual_resolution": False, "framerate": 60, "encoder": "h264enc",
    "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
    "scaling_dpi": 96, "displayPosition": "right",
}


class CursorWindow:
    """An override-redirect window under the pointer whose cursor the test sets.

    The displayed cursor -- the one XFixes reports -- is the pointer window's,
    so the shape is driven from a window this test owns rather than from the
    root, which the session's window manager covers.
    """

    def __init__(self):
        from selkies.Xlib import X
        self.d = H.x_display()
        self.root = self.d.screen().root
        self.font = self.d.open_font("cursor")
        self.win = self.root.create_window(
            300, 300, 200, 200, 0, X.CopyFromParent, X.InputOutput,
            X.CopyFromParent, background_pixel=self.d.screen().white_pixel,
            override_redirect=True)
        self.win.map()
        self.root.warp_pointer(380, 380)
        self.d.sync()
        time.sleep(0.5)

    def set(self, glyph: int) -> None:
        cursor = self.font.create_glyph_cursor(
            self.font, glyph, glyph + 1, (0, 0, 0), (65535, 65535, 65535))
        self.win.change_attributes(cursor=cursor)
        self.d.sync()

    def close(self) -> None:
        self.win.destroy()
        self.d.sync()
        self.d.close()


def curdata(message: str) -> str:
    """The base64 PNG of a ``cursor,`` message, or "" for anything else."""
    if not isinstance(message, str) or not message.startswith("cursor,"):
        return ""
    try:
        return json.loads(message[len("cursor,"):]).get("curdata", "")
    except ValueError:
        return ""


async def collect(ws, seconds: float) -> list:
    """Every distinct cursor sprite the socket delivers within `seconds`."""
    seen = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            message = await asyncio.wait_for(ws.recv(), timeout=0.2)
        except asyncio.TimeoutError:
            continue
        data = curdata(message)
        if data and (not seen or seen[-1] != data):
            seen.append(data)
    return seen


async def drive(res: "H.Results", cursors: CursorWindow) -> None:
    """Connect one client and follow the cursor across the session's sources."""
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    cursors.set(GLYPH_A)
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send("SETTINGS," + json.dumps(SETTINGS))
        seeded = await collect(ws, 4.0)
        res.check("a connecting client is given a cursor", bool(seeded),
                  f"{len(seeded)} sprite(s)")

        cursors.set(GLYPH_B)
        changed = await collect(ws, 4.0)
        res.check("a change while streaming reaches the client",
                  bool(changed) and changed[-1] != seeded[-1],
                  f"{len(changed)} sprite(s)")

        # Stopping the video releases the capture, and with it pixelflux's
        # cursor thread.
        await ws.send("STOP_VIDEO")
        await asyncio.sleep(1.5)
        cursors.set(GLYPH_A)
        quiet = await collect(ws, 2.0)
        res.check("nothing is delivered while nothing captures", not quiet,
                  f"{len(quiet)} sprite(s)")

        await ws.send("START_VIDEO")
        resumed = await collect(ws, 6.0)
        res.check("the resumed capture re-delivers the shape set while it was down",
                  bool(resumed) and resumed[-1] != changed[-1]
                  and resumed[-1] == seeded[-1],
                  f"{len(resumed)} sprite(s)")


def main() -> bool:
    res = H.Results("cursor-delivery")
    H.server_start(mode="websockets", wayland=False)
    cursors = CursorWindow()
    try:
        asyncio.run(drive(res, cursors))
    finally:
        cursors.close()
        H.server_stop()
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
