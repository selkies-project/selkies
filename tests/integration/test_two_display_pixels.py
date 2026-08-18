#!/usr/bin/env python3
"""Each display carries its OWN region of the desktop, proven from the wire.

The layout can be right while the pixels are wrong: a secondary pinned to the
same offset as the primary mirrors it, and a primary sized to the union spans
both halves. This paints a different colour on each display's region and decodes
the JPEG stripes the server actually sends to a client bound to that display, so
neither mistake can pass.

Read at the protocol level rather than through a browser: the Chromium decode
path renders into a worker-owned OffscreenCanvas, whose contents a page-side
drawImage cannot read back reliably.
"""
import asyncio
import io
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C

from typing import Optional

PRIMARY_RGB = (220, 30, 30)
SECONDARY_RGB = (30, 60, 220)
TOLERANCE = 60


def settings_for(display_id: str, position: str = "right") -> dict:
    """Build a jpeg-encoder SETTINGS payload for the display."""
    return {
        "displayId": display_id, "initialClientWidth": 1280, "initialClientHeight": 720,
        "is_manual_resolution_mode": False, "framerate": 30, "encoder": "jpeg",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": position,
    }


def server_layout() -> Optional[dict]:
    """The layout the server last calculated, as `{display_id: rect}`."""
    import ast

    out = subprocess.run(["grep", "-a", "Layout calculated", H.LOG],
                         capture_output=True, text=True).stdout.strip().splitlines()
    if not out:
        return None
    tail = out[-1].split("Layouts: ", 1)
    return ast.literal_eval(tail[1]) if len(tail) > 1 else None


def wait_root_width(width: int, timeout: float = 30) -> Optional[tuple]:
    """Block until the X root has grown to at least the union width."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        got = H.x_root_size()
        if got and got[0] >= width:
            return got
        time.sleep(0.5)
    return None


def paint_regions(layout: dict):
    """Fill each display's region on the root with its own colour.

    Returns the connection; the drawing outlives it, but keeping it open costs
    nothing and keeps the caller symmetric with the window-based helpers.
    """
    from selkies.Xlib import display as xdisp

    d = xdisp.Display(H.TEST_DISPLAY)
    root = d.screen().root
    for did, rect in sorted(layout.items()):
        rgb = PRIMARY_RGB if did == "primary" else SECONDARY_RGB
        gc = root.create_gc(foreground=(rgb[0] << 16) | (rgb[1] << 8) | rgb[2])
        root.fill_rectangle(gc, rect["x"], rect["y"], rect["w"], rect["h"])
    d.sync()
    return d


def mean_rgb(stripes: list) -> Optional[tuple]:
    """Mean colour of the most recently delivered stripe.

    Recency, not size: a screen change in flight produces a transitional frame
    that compresses larger than the settled one, so the biggest stripe reports a
    frame the display has already moved past.
    """
    from PIL import Image

    if not stripes:
        return None
    img = Image.open(io.BytesIO(stripes[-1])).convert("RGB")
    px = list(img.getdata())
    n = len(px)
    return tuple(round(sum(p[i] for p in px) / n) for i in range(3))


async def collect(ws, seconds: float) -> list[bytes]:
    """JPEG stripe payloads received over this socket."""
    stripes = []
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            m = await asyncio.wait_for(ws.recv(), timeout=1.5)
        except asyncio.TimeoutError:
            continue
        if isinstance(m, (bytes, bytearray)) and len(m) > 6 and m[0] == 0x03:
            stripes.append(bytes(m[6:]))
    return stripes


def near(sample: Optional[tuple], rgb: tuple) -> bool:
    return sample is not None and all(abs(sample[i] - rgb[i]) <= TOLERANCE for i in range(3))


async def main() -> bool:
    """Seed two displays, paint their regions, and decode what each receives."""
    import websockets

    res = H.Results("two-display-pixels")
    H.server_start(mode="websockets", wayland=False, extra_env={"SELKIES_USE_CPU": "true"})
    uri = "ws://localhost:{}/api/websockets".format(H.PORT)
    xd = None

    try:
        async with websockets.connect(uri, max_size=None) as wsp:
            await asyncio.wait_for(wsp.recv(), timeout=10)
            await wsp.send("SETTINGS," + json.dumps(settings_for("primary")))
            await asyncio.sleep(3.0)

            async with websockets.connect(uri, max_size=None) as wss:
                await asyncio.wait_for(wss.recv(), timeout=10)
                await wss.send("SETTINGS," + json.dumps(settings_for("display2")))

                res.check("secondary capture started",
                          C.wait_log("SUCCESS: Capture started for 'display2'", timeout=45), "")
                layout = server_layout()
                res.check("both displays laid out",
                          bool(layout) and "display2" in layout, layout)
                if not layout or "display2" not in layout:
                    return res.summary()

                union_w = max(r["x"] + r["w"] for r in layout.values())
                res.check("framebuffer grew to the union",
                          wait_root_width(union_w) is not None, union_w)

                xd = paint_regions(layout)
                await asyncio.sleep(2.0)
                pstripes, sstripes = await asyncio.gather(collect(wsp, 6.0), collect(wss, 6.0))
                res.check("primary delivered frames after paint", len(pstripes) > 0, len(pstripes))
                res.check("secondary delivered frames after paint", len(sstripes) > 0, len(sstripes))

                pc, sc = mean_rgb(pstripes), mean_rgb(sstripes)
                res.check("primary carries its own region", near(pc, PRIMARY_RGB),
                          "{} want {}".format(pc, PRIMARY_RGB))
                res.check("secondary carries its own region", near(sc, SECONDARY_RGB),
                          "{} want {}".format(sc, SECONDARY_RGB))
                res.check("the two displays differ", pc != sc, "{} vs {}".format(pc, sc))
    finally:
        if xd is not None:
            xd.close()
        H.server_stop()
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if asyncio.run(main()) else 1)
