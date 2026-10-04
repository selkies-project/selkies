#!/usr/bin/env python3
"""A Wayland client that opens a popup over a layer surface, as a panel opens a
menu, or over a window, for e2e testing of the pixelflux compositor.

Usage: wl_popup_client.py <socket> [layer|window]   (or POPUP_MODE)

The parent, 300x40 at the top left, is painted PARENT_FILL and the popup,
200x120 hanging below it, POPUP_FILL (ARGB hex). Prints JSONL: "mapped" for
each surface, "popup_configure", and pointer enter, motion and button events
with the surface ("parent" or "popup") they reached.

pywayland carries no wlr-layer-shell, so its bindings are generated at start
from the protocol XML beside this file, in a package next to pywayland's own
wayland and xdg_shell modules, which the generated module imports relatively.
"""
import json
import mmap
import os
import select
import sys
import tempfile
import time

import pywayland.protocol
from pywayland.client import Display
from pywayland.protocol.wayland import WlCompositor, WlSeat, WlShm
from pywayland.protocol.xdg_shell import XdgPositioner, XdgWmBase
from pywayland.scanner import Protocol

SOCKET = sys.argv[1] if len(sys.argv) > 1 else "wayland-1"
MODE = sys.argv[2] if len(sys.argv) > 2 else os.environ.get("POPUP_MODE", "layer")
PARENT_FILL = int(os.environ.get("PARENT_FILL", "ff2060c0"), 16)
POPUP_FILL = int(os.environ.get("POPUP_FILL", "ffe08020"), 16)
DURATION = float(os.environ.get("POPUP_DURATION", "60"))
XML = os.path.join(os.path.dirname(os.path.abspath(__file__)), "protocols", "wlr-layer-shell-unstable-v1.xml")


def emit(kind: str, **kv) -> None:
    """Print one JSONL event line for the test driver to parse."""
    print(json.dumps({"kind": kind, **kv}), flush=True)


gen = tempfile.mkdtemp(prefix="wlgen-")
pkg = os.path.join(gen, "wlext")
os.makedirs(pkg)
open(os.path.join(pkg, "__init__.py"), "w").close()
base = list(pywayland.protocol.__path__)[0]
for name in os.listdir(base):
    if name.split(".")[0] in ("wayland", "xdg_shell"):
        os.symlink(os.path.join(base, name), os.path.join(pkg, name))
Protocol.parse_file(XML).output(pkg, {
    "wl_surface": "wayland", "wl_output": "wayland", "xdg_popup": "xdg_shell",
    "zwlr_layer_shell_v1": "wlr_layer_shell_unstable_v1",
    "zwlr_layer_surface_v1": "wlr_layer_shell_unstable_v1"})
sys.path.insert(0, gen)
from wlext.wlr_layer_shell_unstable_v1 import ZwlrLayerShellV1, ZwlrLayerSurfaceV1  # noqa: E402

display = Display(SOCKET)
display.connect()
registry = display.get_registry()
globals_ = {}
WANT = {"wl_compositor": (WlCompositor, 4), "wl_shm": (WlShm, 1), "wl_seat": (WlSeat, 5),
        "xdg_wm_base": (XdgWmBase, 2), "zwlr_layer_shell_v1": (ZwlrLayerShellV1, 3)}


def on_global(reg, name, iface, version):
    if iface in WANT and iface not in globals_:
        cls, ceiling = WANT[iface]
        globals_[iface] = reg.bind(name, cls, min(version, ceiling))


registry.dispatcher["global"] = on_global
display.roundtrip()
display.roundtrip()
missing = [i for i in WANT if i not in globals_ and (i != "zwlr_layer_shell_v1" or MODE == "layer")]
if missing:
    emit("error", missing=missing)
    sys.exit(2)
globals_["xdg_wm_base"].dispatcher["ping"] = lambda wm, serial: wm.pong(serial)

# Buffers, their pools and backing files live as long as the client.
keep = []


def buffer(width: int, height: int, argb: int):
    """A wl_buffer filled with one ARGB8888 color."""
    size = width * height * 4
    backing = tempfile.TemporaryFile(dir=os.environ.get("XDG_RUNTIME_DIR"))
    os.ftruncate(backing.fileno(), size)
    data = mmap.mmap(backing.fileno(), size)
    data.write(argb.to_bytes(4, "little") * (width * height))
    pool = globals_["wl_shm"].create_pool(backing.fileno(), size)
    buf = pool.create_buffer(0, width, height, width * 4, WlShm.format.argb8888.value)
    keep.append((pool, data, backing))
    return buf


compositor = globals_["wl_compositor"]
parent = compositor.create_surface()
popup_surface = compositor.create_surface()


def show(surface, width: int, height: int, argb: int, tag: str) -> None:
    surface.attach(buffer(width, height, argb), 0, 0)
    surface.damage_buffer(0, 0, width, height)
    surface.commit()
    emit("mapped", surface=tag)


def make_popup(parent_xdg):
    """The 200x120 popup below the parent's top 300x40, as an xdg_popup of
    `parent_xdg`, or of none where a layer surface adopts it."""
    xdg = globals_["xdg_wm_base"].get_xdg_surface(popup_surface)
    positioner = globals_["xdg_wm_base"].create_positioner()
    positioner.set_size(200, 120)
    positioner.set_anchor_rect(0, 0, 300, 40)
    positioner.set_anchor(XdgPositioner.anchor.bottom_left)
    positioner.set_gravity(XdgPositioner.gravity.bottom_right)
    popup = xdg.get_popup(parent_xdg, positioner)

    def configured(surface, serial):
        surface.ack_configure(serial)
        show(popup_surface, 200, 120, POPUP_FILL, "popup")
    xdg.dispatcher["configure"] = configured
    popup.dispatcher["configure"] = lambda p, x, y, w, h: emit("popup_configure", x=x, y=y, w=w, h=h)
    popup.dispatcher["popup_done"] = lambda p: emit("popup_done")
    keep.append((xdg, popup, positioner))
    return popup


opened = []
if MODE == "layer":
    layer = globals_["zwlr_layer_shell_v1"].get_layer_surface(
        parent, None, ZwlrLayerShellV1.layer.top.value, "popup-test")
    layer.set_size(300, 40)
    layer.set_anchor(ZwlrLayerSurfaceV1.anchor.top.value | ZwlrLayerSurfaceV1.anchor.left.value)

    def layer_configured(surface, serial, width, height):
        surface.ack_configure(serial)
        show(parent, 300, 40, PARENT_FILL, "parent")
        if not opened:
            opened.append(True)
            surface.get_popup(make_popup(None))
            popup_surface.commit()
    layer.dispatcher["configure"] = layer_configured
else:
    xdg = globals_["xdg_wm_base"].get_xdg_surface(parent)
    toplevel = xdg.get_toplevel()
    toplevel.set_title("popup-test")

    def window_configured(surface, serial):
        surface.ack_configure(serial)
        show(parent, 300, 200, PARENT_FILL, "parent")
        if not opened:
            opened.append(True)
            make_popup(surface)
            popup_surface.commit()
    xdg.dispatcher["configure"] = window_configured
    toplevel.dispatcher["configure"] = lambda t, w, h, states: None
parent.commit()

pointer = globals_["wl_seat"].get_pointer()
under = {"surface": None}


def tag(surface):
    if surface is None:
        return None
    return "popup" if surface == popup_surface else ("parent" if surface == parent else "other")


def entered(p, serial, surface, x, y):
    under["surface"] = tag(surface)
    emit("ptr_enter", surface=under["surface"], x=x, y=y)


def left(p, serial, surface):
    emit("ptr_leave", surface=tag(surface))
    under["surface"] = None


pointer.dispatcher["enter"] = entered
pointer.dispatcher["leave"] = left
pointer.dispatcher["motion"] = lambda p, t, x, y: emit("ptr_motion", surface=under["surface"], x=x, y=y)
pointer.dispatcher["button"] = lambda p, serial, t, button, state: emit(
    "ptr_button", surface=under["surface"], button=button, state=state)

deadline = time.time() + DURATION
while time.time() < deadline:
    display.flush()
    ready, _, _ = select.select([display.get_fd()], [], [], 0.2)
    if ready and display.dispatch(block=True) < 0:
        break
