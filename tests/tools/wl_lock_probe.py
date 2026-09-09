#!/usr/bin/env python3
"""Wayland pointer-lock client, for asserting what a game holding a lock is told.

Maps a fullscreen surface, takes a `zwp_locked_pointer_v1` on it once the
pointer is inside, and prints JSONL for the lock's confirmation, every
relative-pointer delta and every absolute pointer motion. A compositor that
honours the lock sends the deltas alone; one that moves the pointer as well
sends absolute motion too, which a toolkit turns into a second delta.
"""
import json
import mmap
import os
import signal
import struct
import sys
import tempfile
import time

from pywayland.client import Display
from pywayland.protocol.wayland import WlCompositor, WlOutput, WlSeat, WlShm
from pywayland.protocol.pointer_constraints_unstable_v1 import ZwpPointerConstraintsV1
from pywayland.protocol.relative_pointer_unstable_v1 import ZwpRelativePointerManagerV1
from pywayland.protocol.xdg_shell import XdgWmBase

SOCKET = sys.argv[1] if len(sys.argv) > 1 else "wayland-1"
DURATION = float(os.environ.get("WL_LOCK_DURATION", "40"))

signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))


def emit(kind: str, **kv) -> None:
    print(json.dumps({"kind": kind, **kv}), flush=True)


display = Display(SOCKET)
display.connect()
registry = display.get_registry()
handles = {}
WANTED = {
    "wl_compositor": ("comp", WlCompositor, 4),
    "wl_shm": ("shm", WlShm, 1),
    "xdg_wm_base": ("xdg", XdgWmBase, 1),
    "wl_seat": ("seat", WlSeat, 5),
    "wl_output": ("output", WlOutput, 1),
    "zwp_pointer_constraints_v1": ("constraints", ZwpPointerConstraintsV1, 1),
    "zwp_relative_pointer_manager_v1": ("relative", ZwpRelativePointerManagerV1, 1),
}


def on_global(reg, name, iface, version):
    if iface in WANTED:
        key, cls, cap = WANTED[iface]
        handles[key] = reg.bind(name, cls, min(version, cap))


registry.dispatcher["global"] = on_global
registry.dispatcher["global_remove"] = lambda *a: None
display.dispatch(block=True)
display.roundtrip()

missing = [k for k in ("comp", "shm", "xdg", "seat", "constraints", "relative") if k not in handles]
if missing:
    emit("unavailable", missing=missing)
    sys.exit(0)

surf = handles["comp"].create_surface()
xdg_surf = handles["xdg"].get_xdg_surface(surf)
toplevel = xdg_surf.get_toplevel()
toplevel.set_title("selkies-wl-lock-probe")
configured, asked = [], [0, 0]
xdg_surf.dispatcher["configure"] = lambda xs, serial: (xs.ack_configure(serial), configured.append(serial))
toplevel.dispatcher["configure"] = lambda _tl, w, h, _s: asked.__setitem__(slice(0, 2), [w, h]) if w and h else None
toplevel.set_fullscreen(handles.get("output"))
surf.commit()
for _ in range(30):
    if display.dispatch(block=False, queue=None) < 0:
        break
    display.roundtrip()
    if configured:
        break
    time.sleep(0.05)

W, H = (asked[0], asked[1]) if asked[0] else (1280, 720)
stride, size = W * 4, W * 4 * H
_tmp = tempfile.TemporaryFile(prefix="wl-lock-buf")
os.ftruncate(_tmp.fileno(), size)
with mmap.mmap(_tmp.fileno(), size) as m:
    m.write(struct.pack("<I", 0xFF101010) * (W * H))
pool = handles["shm"].create_pool(_tmp.fileno(), size)
buf = pool.create_buffer(0, W, H, stride, 0)
pool.destroy()
surf.attach(buf, 0, 0)
surf.commit()
display.roundtrip()

ptr = handles["seat"].get_pointer()
rel = handles["relative"].get_relative_pointer(ptr)
lock = {"obj": None}


def take_lock() -> None:
    """One lock for the life of the probe, held whenever the pointer is inside."""
    if lock["obj"] is not None:
        return
    obj = handles["constraints"].lock_pointer(
        surf, ptr, None, ZwpPointerConstraintsV1.lifetime.persistent)
    obj.dispatcher["locked"] = lambda _o: emit("locked")
    obj.dispatcher["unlocked"] = lambda _o: emit("unlocked")
    lock["obj"] = obj
    surf.commit()


ptr.dispatcher["enter"] = lambda _p, serial, s, sx, sy: (emit("enter", x=float(sx), y=float(sy)), take_lock())
ptr.dispatcher["leave"] = lambda _p, serial, s: emit("leave")
ptr.dispatcher["motion"] = lambda _p, t, sx, sy: emit("motion", x=float(sx), y=float(sy))
ptr.dispatcher["button"] = lambda _p, serial, t, b, st: emit("button", button=b, down=bool(st))
ptr.dispatcher["axis"] = lambda *a: None
rel.dispatcher["relative_motion"] = (
    lambda _r, uhi, ulo, dx, dy, udx, udy: emit("relative", dx=float(dx), dy=float(dy)))
display.roundtrip()
emit("mapped", w=W, h=H)

end = time.time() + DURATION
while time.time() < end:
    if display.dispatch(block=True) < 0:
        break
emit("done")
