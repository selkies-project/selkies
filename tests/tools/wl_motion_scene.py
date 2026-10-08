#!/usr/bin/env python3
"""motion_scene.py's moving scene as a fullscreen client of the pixelflux compositor.

Draws what motion_scene.py draws on X11, frame for frame and at the same places, into
wl_shm buffers at the size the compositor configures, again after each resize: the
background, the strip of squares spelling the frame index, the scrolling checkerboard
band, the stepping bar and the band of noise along the bottom. Prints
{"kind": "mapped", "w", "h"} whenever a frame of a new size is shown, and exits after
WLOBS_DURATION seconds or on SIGTERM.

    XDG_RUNTIME_DIR=... wl_motion_scene.py SOCKET
"""
import json
import mmap
import os
import random
import select
import signal
import sys
import tempfile
import time

from pywayland.client import Display
from pywayland.protocol.wayland import WlCompositor, WlShm
from pywayland.protocol.xdg_shell import XdgWmBase

from motion_scene import (BAND_H, BAND_STEP, BAND_Y, BAR_STEP, BAR_W, CHECK, CODE_BITS, CODE_GAP, CODE_SIZE,
                          CODE_X, CODE_Y, NOISE_H, NOISE_IMAGES)

SOCKET = sys.argv[1] if len(sys.argv) > 1 else "wayland-1"
DURATION = float(os.environ.get("WLOBS_DURATION", "60"))
FPS = 60.0
BUFFERS = 3
# XRGB8888 is little-endian: blue, green, red, unused.
BACKGROUND = bytes((0x78, 0x28, 0x1E, 0xFF))
BAR = bytes((0x28, 0x3C, 0xDC, 0xFF))
BLACK = bytes((0, 0, 0, 0xFF))
WHITE = bytes((0xFF, 0xFF, 0xFF, 0xFF))
PERIOD = 2 * CHECK

signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
display = Display(SOCKET)
display.connect()
reg = display.get_registry()
g = {}


def on_global(r, name, iface, version):
    if iface == "wl_compositor" and "comp" not in g:
        g["comp"] = r.bind(name, WlCompositor, min(version, 4))
    elif iface == "wl_shm" and "shm" not in g:
        g["shm"] = r.bind(name, WlShm, 1)
    elif iface == "xdg_wm_base" and "xdg" not in g:
        g["xdg"] = r.bind(name, XdgWmBase, 1)


reg.dispatcher["global"] = on_global
reg.dispatcher["global_remove"] = lambda *a: None
display.roundtrip()
g["xdg"].dispatcher["ping"] = lambda wm, serial: wm.pong(serial)
surface = g["comp"].create_surface()
xs = g["xdg"].get_xdg_surface(surface)
toplevel = xs.get_toplevel()
toplevel.set_title("motion-scene")
asked = {"size": (1280, 720), "configured": False}


def on_toplevel_configure(_, w, h, states):
    if w > 0 and h > 0:
        asked["size"] = (w, h)


def on_configure(x, serial):
    x.ack_configure(serial)
    asked["configured"] = True


toplevel.dispatcher["configure"] = on_toplevel_configure
toplevel.dispatcher["close"] = lambda _: sys.exit(0)
xs.dispatcher["configure"] = on_configure
toplevel.set_fullscreen(None)
surface.commit()
while not asked["configured"]:
    display.dispatch(block=True)


class Canvas:
    """Three buffers of one size, the band and noise rows drawn from, and which buffers the compositor holds."""

    def __init__(self, w: int, h: int) -> None:
        self.w, self.h, self.stride = w, h, w * 4
        self.noise_h = min(NOISE_H, h)
        self.size = self.stride * h
        fd, path = tempfile.mkstemp(dir=os.environ.get("XDG_RUNTIME_DIR", "/tmp"))
        os.unlink(path)
        os.ftruncate(fd, BUFFERS * self.size)
        self.mm = mmap.mmap(fd, BUFFERS * self.size)
        self.mm[:] = BACKGROUND * (w * h * BUFFERS)
        self.pool = g["shm"].create_pool(fd, BUFFERS * self.size)
        os.close(fd)
        self.buffers = [self.pool.create_buffer(b * self.size, w, h, self.stride, WlShm.format.xrgb8888.value)
                        for b in range(BUFFERS)]
        self.free = [True] * BUFFERS
        for b, buf in enumerate(self.buffers):
            buf.dispatcher["release"] = lambda _, b=b: self.free.__setitem__(b, True)
        self.band_rows = [b"".join(WHITE if ((x // CHECK) + (y // CHECK)) % 2 == 0 else BLACK
                                   for x in range(w + PERIOD)) for y in range(BAND_H)]
        rng = random.Random(1)
        self.noises = [[rng.randbytes(2 * self.stride) for _ in range(self.noise_h)]
                       for _ in range(NOISE_IMAGES if self.noise_h else 0)]

    def close(self) -> None:
        for buf in self.buffers:
            buf.destroy()
        self.pool.destroy()
        self.mm.close()

    def draw(self, index: int) -> bool:
        """The frame for `index` into a free buffer, committed; False where none is free."""
        b = next((i for i in range(BUFFERS) if self.free[i]), None)
        if b is None:
            return False
        self.free[b] = False
        w, stride, mm, base = self.w, self.stride, self.mm, b * self.size
        code = bytearray(BACKGROUND * w)
        for i in range(CODE_BITS):
            x = CODE_X + i * (CODE_SIZE + CODE_GAP)
            code[x * 4:(x + CODE_SIZE) * 4] = (WHITE if (index >> i) & 1 else BLACK) * CODE_SIZE
        for y in range(CODE_Y, CODE_Y + CODE_SIZE):
            mm[base + y * stride:base + (y + 1) * stride] = code
        off = (index * BAND_STEP) % PERIOD
        for y in range(BAND_H):
            at = base + (BAND_Y + y) * stride
            mm[at:at + stride] = self.band_rows[y][off * 4:(off + w) * 4]
        bar_bottom = self.h - self.noise_h
        x = (index * BAR_STEP) % (w - BAR_W)
        bar = BACKGROUND * x + BAR * BAR_W + BACKGROUND * (w - x - BAR_W)
        for y in range(BAND_Y + BAND_H, bar_bottom):
            mm[base + y * stride:base + (y + 1) * stride] = bar
        if self.noises:
            rows, off = self.noises[index % NOISE_IMAGES], (index * 97) % w
            for y in range(self.noise_h):
                at = base + (bar_bottom + y) * stride
                mm[at:at + stride] = rows[y][off * 4:(off + w) * 4]
        surface.attach(self.buffers[b], 0, 0)
        surface.damage_buffer(0, CODE_Y, w, self.h - CODE_Y)
        surface.commit()
        display.flush()
        return True


canvas = None
index = 0
fdw = display.get_fd()
end = time.monotonic() + DURATION
next_tick = time.monotonic()
while time.monotonic() < end:
    if canvas is None or (canvas.w, canvas.h) != asked["size"]:
        if canvas is not None:
            canvas.close()
        canvas = Canvas(*asked["size"])
        canvas.draw(index)
        display.roundtrip()
        print(json.dumps({"kind": "mapped", "w": canvas.w, "h": canvas.h}), flush=True)
    if select.select([fdw], [], [], max(0.0, next_tick - time.monotonic()))[0]:
        display.dispatch(block=True)
    if time.monotonic() >= next_tick:
        if canvas.draw((index + 1) % (1 << CODE_BITS)):
            index = (index + 1) % (1 << CODE_BITS)
        next_tick = max(next_tick + 1 / FPS, time.monotonic())
    display.flush()
