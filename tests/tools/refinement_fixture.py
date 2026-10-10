# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Private deterministic SHM source with bounded JSON paint commands on stdin.

A committed record identifies the producer image; it is not a capture or browser
presentation acknowledgement. Retain at most 32 images in this short probe.
"""
import argparse
import json
import mmap
import os
import select
import signal
import tempfile
import time
import sys
from typing import Any

import numpy as np

def pixels(width: int, height: int, seed: int) -> np.ndarray:
    """Independent integer-defined opaque RGBA source shared with the oracle."""
    x = np.arange(width, dtype=np.uint32)[None, :]
    y = np.arange(height, dtype=np.uint32)[:, None]
    value = x * np.uint32(374761393) + y * np.uint32(668265263) + np.uint32(seed)
    value = (value ^ (value >> np.uint32(13))) * np.uint32(1274126177)
    value ^= value >> np.uint32(16)
    rgba = np.empty((height, width, 4), dtype=np.uint8)
    for channel, shift in enumerate((0, 8, 16)):
        rgba[:, :, channel] = (value >> np.uint32(shift)).astype(np.uint8)
    rgba[:, :, 3] = 255
    return rgba


def source_pixels(width: int, height: int, seed: int) -> np.ndarray:
    """Include geometry in every pixel so an old buffer cannot pass after resize."""
    return pixels(width, height, (seed + width * 1009 + height * 9176) & 0xFFFFFFFF)


def main() -> None:
    """Handle configure/ack/commit while keeping all buffers alive until exit."""
    from pywayland.client import Display
    from pywayland.protocol.wayland import WlCompositor, WlOutput, WlSeat, WlShm
    from pywayland.protocol.xdg_shell import XdgWmBase

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--output-x", type=int, required=True)
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--title", required=True)
    args = parser.parse_args()
    state = {"stop": False, "size": (0, 0), "generation": 0, "seed": args.seed}
    signal.signal(signal.SIGTERM, lambda *_: state.update(stop=True))
    display = Display(args.socket)
    display.connect()
    registry = display.get_registry()
    handles, outputs, owned = {}, [], []

    def bind(reg: Any, name: int, interface: str, version: int) -> None:
        """Bind the minimal compositor interfaces and record output positions."""
        choices = {"wl_compositor": ("compositor", WlCompositor, 4),
                   "wl_shm": ("shm", WlShm, 1), "wl_seat": ("seat", WlSeat, 5),
                   "xdg_wm_base": ("xdg", XdgWmBase, 1)}
        if interface in choices:
            key, cls, ceiling = choices[interface]
            if key not in handles:
                handles[key] = reg.bind(name, cls, min(version, ceiling))
        if interface == "wl_output":
            output = reg.bind(name, WlOutput, min(version, 2))
            entry = {"output": output, "x": None}
            output.dispatcher["geometry"] = lambda _, x, *rest: entry.update(x=x)
            outputs.append(entry)

    registry.dispatcher["global"] = bind
    registry.dispatcher["global_remove"] = lambda *_: None
    display.roundtrip()
    display.roundtrip()
    handles["xdg"].dispatcher["ping"] = lambda wm, serial: wm.pong(serial)
    target = next(item["output"] for item in outputs if item["x"] == args.output_x)

    def buffer_for(rgba: np.ndarray) -> Any:
        """Create an owned SHM buffer whose lifetime covers this bounded probe."""
        height, width = rgba.shape[:2]
        backing = tempfile.TemporaryFile(dir=os.environ["XDG_RUNTIME_DIR"])
        os.ftruncate(backing.fileno(), width * height * 4)
        mapping = mmap.mmap(backing.fileno(), width * height * 4)
        mapping[:] = rgba[:, :, [2, 1, 0, 3]].tobytes()
        pool = handles["shm"].create_pool(backing.fileno(), width * height * 4)
        buffer = pool.create_buffer(0, width, height, width * 4, WlShm.format.argb8888.value)
        owned.append((buffer, pool, mapping, backing))
        return buffer

    cursor = handles["compositor"].create_surface()
    cursor.attach(buffer_for(np.zeros((8, 8, 4), dtype=np.uint8)), 0, 0)
    cursor.damage_buffer(0, 0, 8, 8)
    cursor.commit()
    pointer = handles["seat"].get_pointer()
    pointer.dispatcher["enter"] = lambda pointer, serial, *rest: pointer.set_cursor(serial, cursor, 0, 0)
    surface = handles["compositor"].create_surface()
    xdg_surface = handles["xdg"].get_xdg_surface(surface)
    toplevel = xdg_surface.get_toplevel()
    toplevel.set_title(args.title)
    toplevel.set_app_id(args.title)
    toplevel.dispatcher["configure"] = lambda _, w, h, states: state.update(size=(w, h)) if w and h else None
    toplevel.dispatcher["close"] = lambda *_: state.update(stop=True)

    def paint() -> None:
        """Commit a new full image and report its producer generation."""
        if state["generation"] >= 32:
            raise RuntimeError("Bounded fixture exhausted")
        width, height = state["size"]
        if width <= 0 or height <= 0:
            raise ValueError("The compositor configured invalid dimensions")
        callback = surface.frame()
        generation = state["generation"] + 1
        state["generation"] = generation
        callback.dispatcher["done"] = lambda _, stamp: print(json.dumps({
            "kind": "frame-done", "generation": generation, "size": [width, height],
            "monotonic_ns": time.monotonic_ns()}), flush=True)
        owned.append(callback)
        surface.attach(buffer_for(source_pixels(width, height, state["seed"])), 0, 0)
        surface.damage_buffer(0, 0, width, height)
        surface.commit()
        print(json.dumps({"kind": "committed", "size": [width, height],
                          "generation": generation, "seed": state["seed"],
                          "monotonic_ns": time.monotonic_ns()}), flush=True)
        display.flush()

    def configured(xdg: Any, serial: int) -> None:
        """Acknowledge the compositor geometry before painting the known source."""
        xdg.ack_configure(serial)
        paint()

    xdg_surface.dispatcher["configure"] = configured
    toplevel.set_fullscreen(target)
    surface.commit()
    display.flush()
    try:
        while not state["stop"]:
            ready = select.select([display.get_fd(), sys.stdin], [], [], 0.2)[0]
            if display.get_fd() in ready:
                display.dispatch(block=True)
            if sys.stdin in ready:
                line = sys.stdin.readline()
                if not line:
                    break
                command = json.loads(line)
                seed = command.get("seed")
                if type(seed) is not int or not 0 <= seed <= 0xFFFFFFFF:
                    raise ValueError("Invalid fixture seed")
                state["seed"] = seed
                paint()
            display.flush()
    finally:
        display.disconnect()


if __name__ == "__main__":
    main()
