#!/usr/bin/env python3
"""The cursor a locked page cannot draw, drawn by the display the pointer is on.

A pointer lock leaves the client with deltas and no positions, so it hands the
cursor to the server (`SET_NATIVE_CURSOR_RENDERING,1`) rather than placing an
overlay of its own. Neither half of that is display-aware: the tunable is the
session's, every capture composites the sprite from the pointer's root-relative
position, and the deltas move the one remote pointer. Locking a single page
therefore covers the whole desktop, and the cursor crosses onto the neighbour's
stream on its own.

Read from the JPEG stripes each display's client is sent, because that is where
the drawing is; a page cannot read its own decode back (test_two_display_pixels
says why). The lock is taken on the primary's connection alone, and what has to
hold is that its deltas carry the one pointer across the seam, that the
neighbour then draws the cursor at the position they reached without having
asked for anything, that the primary stops drawing it, and that unlocking hands
the drawing back to the client -- where pointer motion damages no frame at all.

The X11 backend is what this reads, where each capture composites the sprite
into its own region of the root; the Wayland compositor paints its cursor per
output under the same tunable, and its seam is test_wayland_seam's.

Usage: python3 tests/integration/test_locked_cursor_across_displays.py
"""
import asyncio
import io
import json
import os
import sys
import time
from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_two_display_pixels as TDP

try:
    import websockets
except ImportError:
    H.skip_suite("websockets is not installed")

# A solid black square, so no theme cursor can be mistaken for it and no JPEG
# stripe can lose it against the flat colour a display's region is painted.
SPRITE = 64
# Distance from that colour which counts as the sprite; ringing on a flat field
# stays an order of magnitude below it.
SPRITE_TOL = 40
START = (400, 300)
# How far past the seam the deltas land the pointer: more than the sprite's
# half, so one display draws all of it.
OVER = 320
STEP = 100
# A move within the neighbour, small enough to leave the sprite whole on it.
NUDGE = (40, 30)
# How far a repainted band is shifted from its display's colour: enough for the
# capture to send the band, too little to read as a sprite.
SHADE = 12


async def collect(ws, seconds: float) -> list:
    """`(frame id, y of the band, jpeg)` for each stripe this socket receives."""
    out = []
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            m = await asyncio.wait_for(ws.recv(), timeout=1.0)
        except asyncio.TimeoutError:
            continue
        if isinstance(m, (bytes, bytearray)) and len(m) > 6 and m[0] == 0x03:
            out.append((int.from_bytes(m[2:4], "big"),
                        int.from_bytes(m[4:6], "big"), bytes(m[6:])))
    return out


def frame_boxes(frames: list, bg: tuple) -> list:
    """`(frame id, sprite box or None)` per delivered frame, oldest first, the
    bands of one frame merged into a single box in the display's own pixels."""
    from PIL import Image, ImageChops

    def band(jpeg: bytes, y: int):
        img = Image.open(io.BytesIO(jpeg)).convert("RGB")
        diff = ImageChops.difference(img, Image.new("RGB", img.size, bg)).convert("L")
        found = diff.point(lambda v: 255 if v > SPRITE_TOL else 0).getbbox()
        return (found[0], y + found[1], found[2], y + found[3]) if found else None

    def merge(a, b):
        if a is None or b is None:
            return a or b
        return (min(a[0], b[0]), min(a[1], b[1]), max(a[2], b[2]), max(a[3], b[3]))

    out: list = []
    for fid, y, jpeg in frames:
        box = band(jpeg, y)
        if out and out[-1][0] == fid:
            out[-1] = (fid, merge(out[-1][1], box))
        else:
            out.append((fid, box))
    return out


def sprite_box(frames: list, bg: tuple) -> Optional[tuple]:
    """Where the sprite is, from the newest frame that carries it.

    Newest, not largest: a frame still in flight shows the sprite where it was,
    and only the newest one says where it is now.
    """
    return next((box for _, box in reversed(frame_boxes(frames, bg)) if box), None)


def still_drawn(frames: list, bg: tuple) -> bool:
    """Whether the display is still drawing the sprite, which only its newest
    frame can say: one frame that carried it keeps `sprite_box` answering, so
    absence is readable in the frame that vacated it and nowhere else."""
    boxes = frame_boxes(frames, bg)
    return bool(boxes) and boxes[-1][1] is not None


def drawn_at(box: Optional[tuple], where: tuple, slack: int = 6) -> bool:
    """Whether a sprite box is the cursor standing at `where` on its display.

    The box has to hold that point rather than centre on it, since the hotspot
    is wherever the cursor in use puts it, and be no larger than the cursor set.
    """
    if box is None:
        return False
    return (box[0] - slack <= where[0] <= box[2] + slack
            and box[1] - slack <= where[1] <= box[3] + slack
            and box[2] - box[0] <= SPRITE + slack and box[3] - box[1] <= SPRITE + slack)


def cursor_window(size: int):
    """An input-only window over the desktop carrying a solid cursor of `size`
    pixels a side, so the displayed cursor is this suite's rather than whatever
    the desktop last set, and nothing covers the painted regions.

    The connection stays open for the run: the window and its cursor are freed
    with it, and the desktop's own cursor comes back.
    """
    from selkies.Xlib import X

    d = H.x_display()
    root = d.screen().root
    geom = root.get_geometry()
    shape = root.create_pixmap(size, size, 1)
    gc = shape.create_gc(foreground=1, background=0)
    shape.fill_rectangle(gc, 0, 0, size, size)
    win = root.create_window(
        0, 0, geom.width, geom.height, 0, 0, X.InputOnly, X.CopyFromParent,
        override_redirect=True,
        cursor=shape.create_cursor(shape, (0, 0, 0), (65535, 65535, 65535),
                                   size // 2, size // 2))
    win.map()
    win.configure(stack_mode=X.Above)
    d.sync()
    gc.free()
    shape.free()
    return d


def dirty_band(d, rect: dict, y: int, rgb: tuple, height: int = SPRITE * 2) -> None:
    """Repaint a band of a display's region in a shade of its own colour.

    A display sends only what changed, so silence cannot tell a cursor that has
    left from one still standing there. The shade -- too near the colour to
    read as a sprite -- makes the band arrive, and what it carries is then an
    answer rather than a silence.
    """
    shaded = (rgb[0], min(255, rgb[1] + SHADE), rgb[2])
    root = d.screen().root
    gc = root.create_gc(foreground=(shaded[0] << 16) | (shaded[1] << 8) | shaded[2])
    root.fill_rectangle(gc, rect["x"], max(0, y - height // 2), rect["w"], height)
    d.sync()
    gc.free()


async def settle(ws, seconds: float = 2.5) -> None:
    """Drain what a change already in flight sends, so the next window is the
    answer to the next move alone."""
    await collect(ws, seconds)


async def main() -> bool:
    """Two displays, one pointer, and the sprite followed from stream to stream."""
    res = H.Results("locked-cursor-across-displays")
    H.server_start(mode="websockets", wayland=False, extra_env={"SELKIES_USE_CPU": "true"})
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    painted = cursor = None

    try:
        async with websockets.connect(uri, max_size=None) as wsp:
            await asyncio.wait_for(wsp.recv(), timeout=10)
            await wsp.send("SETTINGS," + json.dumps(TDP.settings_for("primary")))
            await asyncio.sleep(3.0)

            async with websockets.connect(uri, max_size=None) as wss:
                await asyncio.wait_for(wss.recv(), timeout=10)
                await wss.send("SETTINGS," + json.dumps(TDP.settings_for("display2")))
                res.check("the neighbour's capture started",
                          C.wait_log("SUCCESS: Capture started for 'display2'", timeout=45), "")
                layout = TDP.server_layout()
                res.check("both displays laid out",
                          bool(layout) and "display2" in layout, layout)
                if not layout or "display2" not in layout:
                    return res.summary()
                seam = layout["display2"]["x"]
                union_w = max(r["x"] + r["w"] for r in layout.values())
                res.check("the framebuffer grew to the union",
                          TDP.wait_root_width(union_w) is not None, union_w)

                painted = TDP.paint_regions(layout)
                cursor = cursor_window(SPRITE)
                await wsp.send(f"m,{START[0]},{START[1]},0,0")
                await asyncio.sleep(2.0)

                # The client draws the cursor itself until a lock takes its
                # positions away, and out-of-band delivery is what makes that
                # free: the pointer moving over static content damages nothing.
                await asyncio.gather(settle(wsp), settle(wss))
                await wsp.send("m2,40,20,0,0")
                await asyncio.sleep(0.5)
                idle = await asyncio.gather(collect(wsp, 4.0), collect(wss, 4.0))
                res.check("a pointer the client draws sends no frames at all",
                          not idle[0] and not idle[1],
                          f"primary {len(idle[0])} frames, neighbour {len(idle[1])}")

                # What a lock asks for, from the locked page's connection alone.
                await wsp.send("SET_NATIVE_CURSOR_RENDERING,1")
                res.check("the lock's request reaches the session",
                          C.wait_log("Received SET_NATIVE_CURSOR_RENDERING: True", timeout=15), "")
                await asyncio.gather(settle(wsp, 5.0), settle(wss, 5.0))
                dirty_band(painted, layout["display2"], START[1], TDP.SECONDARY_RGB)
                await wsp.send(f"m,{START[0]},{START[1]},0,0")
                await asyncio.sleep(0.8)
                shown = await asyncio.gather(collect(wsp, 4.0), collect(wss, 4.0))
                res.check("the display the pointer is on draws the cursor",
                          drawn_at(sprite_box(shown[0], TDP.PRIMARY_RGB), START),
                          f"{sprite_box(shown[0], TDP.PRIMARY_RGB)} at {START}")
                res.check("the display it is away from draws none",
                          sprite_box(shown[1], TDP.SECONDARY_RGB) is None,
                          f"{sprite_box(shown[1], TDP.SECONDARY_RGB)} in {len(shown[1])} frames")

                # The locked page's own messages: deltas, and only deltas.
                await asyncio.gather(settle(wsp), settle(wss))
                travel = seam + OVER - START[0]
                for step in [STEP] * (travel // STEP) + [travel % STEP]:
                    if step:
                        await wsp.send(f"m2,{step},0,0,0")
                        await asyncio.sleep(0.05)
                await asyncio.sleep(1.0)
                pos = C.x11_mouse_pos()
                rect = layout["display2"]
                res.check("the deltas carry the one pointer onto the neighbour",
                          rect["x"] <= pos[0] < rect["x"] + rect["w"]
                          and abs(pos[0] - (seam + OVER)) <= 2,
                          f"{pos} in {rect}")
                # A nudge once the crossing has drained, so what each display
                # sends answers for where the pointer is now rather than for
                # the frames the travel left in flight.
                await asyncio.gather(settle(wsp), settle(wss))
                dirty_band(painted, layout["primary"], START[1], TDP.PRIMARY_RGB)
                await wsp.send(f"m2,{-NUDGE[0]},{NUDGE[1]},0,0")
                await asyncio.sleep(0.8)
                crossed = await asyncio.gather(collect(wsp, 4.0), collect(wss, 4.0))
                landed = (OVER - NUDGE[0], START[1] + NUDGE[1])
                res.check("the neighbour draws the cursor where the deltas reached it",
                          drawn_at(sprite_box(crossed[1], TDP.SECONDARY_RGB), landed),
                          f"{sprite_box(crossed[1], TDP.SECONDARY_RGB)} at {landed}")
                res.check("the display it left stops drawing it",
                          not still_drawn(crossed[0], TDP.PRIMARY_RGB),
                          f"{frame_boxes(crossed[0], TDP.PRIMARY_RGB)[-1:]} "
                          f"in {len(crossed[0])} frames")

                # Unlocking gives the client its positions back, and the drawing with them.
                await wsp.send("SET_NATIVE_CURSOR_RENDERING,0")
                res.check("the unlock's request reaches the session",
                          C.wait_log("Received SET_NATIVE_CURSOR_RENDERING: False", timeout=15), "")
                await asyncio.gather(settle(wsp, 5.0), settle(wss, 5.0))
                for did, rgb in (("primary", TDP.PRIMARY_RGB), ("display2", TDP.SECONDARY_RGB)):
                    dirty_band(painted, layout[did], landed[1], rgb)
                await asyncio.sleep(0.8)
                back = await asyncio.gather(collect(wsp, 4.0), collect(wss, 4.0))
                res.check("unlocking hands the drawing back to the client",
                          sprite_box(back[0], TDP.PRIMARY_RGB) is None
                          and sprite_box(back[1], TDP.SECONDARY_RGB) is None
                          and back[0] and back[1],
                          f"{sprite_box(back[1], TDP.SECONDARY_RGB)} in "
                          f"{len(back[0])} and {len(back[1])} frames")
    finally:
        for d in (cursor, painted):
            if d is not None:
                try:
                    d.close()
                except Exception:
                    pass
        H.server_stop()
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if asyncio.run(main()) else 1)
