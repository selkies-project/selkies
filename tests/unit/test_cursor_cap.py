#!/usr/bin/env python3
"""An application's own cursor is delivered whole up to what a browser shows.

The delivery cap bounds the sprite that reaches the client; it is not the
theme size. A game's 128px pointer therefore travels at 128, the most a
browser accepts as a cursor image, while a sprite past that is brought down
to the cap. The XFixes encode path is driven with synthetic cursor images; the
pixelflux paths carry the same rule in their own tests.
"""
import base64
import inspect
import io
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

from PIL import Image  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402

res = H.Results("cursor-cap")


class XCursor:
    """An XFixes cursor image: opaque black, premultiplied ARGB words."""

    def __init__(self, size: int, serial: int) -> None:
        self.width = self.height = size
        self.xhot = self.yhot = size // 2
        self.cursor_serial = serial
        self.cursor_image = [0xFF000000] * (size * size)


def handler(cap: int) -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h.cursor_size_cap = cap
    h.cursor_debug = False
    h._cursor_msg_cache = None
    return h


def delivered(msg: dict) -> tuple:
    im = Image.open(io.BytesIO(base64.b64decode(msg["curdata"])))
    return im.size, (int(msg["hotx"]), int(msg["hoty"]))


default_cap = inspect.signature(WebRTCInput.__init__).parameters["max_cursor_size"].default
res.check("the default delivery cap is a browser's cursor ceiling", default_cap == 128, default_cap)

size, hot = delivered(handler(default_cap).cursor_to_msg(XCursor(128, 1)))
res.check("a 128px application cursor is delivered whole at the default cap",
          size == (128, 128) and hot == (64, 64), (size, hot))

size, hot = delivered(handler(default_cap).cursor_to_msg(XCursor(256, 2)))
res.check("a sprite past the cap is brought down to it, hotspot with it",
          size == (128, 128) and hot == (64, 64), (size, hot))

size, hot = delivered(handler(default_cap * 2).cursor_to_msg(XCursor(256, 3)))
res.check("a raised cap (a higher DPI) lets the larger sprite through",
          size == (256, 256) and hot == (128, 128), (size, hot))

size, _ = delivered(handler(default_cap).cursor_to_msg(XCursor(24, 4)))
res.check("a theme-sized cursor is untouched by the cap", size == (24, 24), size)

sys.exit(0 if res.summary() else 1)
