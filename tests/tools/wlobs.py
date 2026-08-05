#!/usr/bin/env python3
"""Wayland observation client for e2e testing of the pixelflux compositor.

Connects to a compositor socket, maps a tiny xdg_toplevel surface, and listens
for seat keyboard/pointer events plus clipboard offers. Prints JSONL events on
stdout so the test driver can assert input/clipboard parity vs X11.
"""
import json
import os
import sys
import tempfile
import time

from pywayland.client import Display
from pywayland.protocol.wayland import (
    WlCompositor, WlSeat, WlShm, WlDataDeviceManager, WlOutput,
)
from pywayland.protocol.xdg_shell import XdgWmBase

SOCKET = sys.argv[1] if len(sys.argv) > 1 else "wayland-1"
DURATION = float(os.environ.get("WLOBS_DURATION", "25"))


def emit(kind, **kv):
    print(json.dumps({"kind": kind, **kv}), flush=True)


display = Display(SOCKET)
display.connect()
registry = display.get_registry()

handles = {
    "seat": None, "comp": None, "shm": None,
    "xdg": None, "ddm": None, "seat_iface": None, "seat_version": 1,
    "dd": None, "output": None,
}


def on_global(reg, name, iface, version):
    if iface == "wl_seat" and handles["seat_iface"] is None:
        handles["seat_iface"] = (name, min(version, 7))
    elif iface == "wl_compositor" and handles["comp"] is None:
        handles["comp"] = reg.bind(name, WlCompositor, min(version, 4))
    elif iface == "wl_shm" and handles["shm"] is None:
        handles["shm"] = reg.bind(name, WlShm, version)
    elif iface == "xdg_wm_base" and handles["xdg"] is None:
        handles["xdg"] = reg.bind(name, XdgWmBase, version)
    elif iface == "wl_data_device_manager" and handles["ddm"] is None:
        handles["ddm"] = reg.bind(name, WlDataDeviceManager, version)
    elif iface == "wl_output" and handles["output"] is None:
        handles["output"] = reg.bind(name, WlOutput, version)


def on_global_remove(reg, name):
    pass


registry.dispatcher["global"] = on_global
registry.dispatcher["global_remove"] = on_global_remove
display.dispatch(block=True)
display.roundtrip()

if handles["seat_iface"] is None:
    emit("fatal", error="no wl_seat")
    sys.exit(1)
name, ver = handles["seat_iface"]
seat = registry.bind(name, WlSeat, ver)


def kbd_keymap(kbd, fmt, fd, size):
    os.close(fd)


def kbd_enter(kbd, serial, surface, keys):
    emit("kbd_enter")


def kbd_leave(kbd, serial, surface):
    emit("kbd_leave")


def kbd_key(kbd, serial, time_ms, key, statev):
    emit("kbd_key", key=key, state=statev)


def kbd_mods(kbd, serial, dep, lat, lock, group):
    emit("kbd_mods", dep=dep, lat=lat, lock=lock, group=group)


def ptr_enter(ptr, serial, surface, sx, sy):
    emit("ptr_enter", x=sx, y=sy)


def ptr_leave(ptr, serial, surface):
    emit("ptr_leave")


def ptr_motion(ptr, time_ms, sx, sy):
    emit("ptr_motion", x=sx, y=sy)


def ptr_button(ptr, serial, time_ms, button, statev):
    emit("ptr_button", button=button, state=statev)


def ptr_axis(ptr, time_ms, axis, value):
    emit("ptr_axis", axis=axis, value=value)


def offer_mimes(offer):
    out = {"mimes": []}
    def _on_offer(of, mime):
        out["mimes"].append(mime)
    offer.dispatcher["offer"] = _on_offer
    offer.dispatcher["source_actions"] = lambda *a: None
    offer.dispatcher["action"] = lambda *a: None
    return out


def dd_offer(dd, offer):
    mo = offer_mimes(offer)
    emit("dd_offer", mimes=mo["mimes"])


def dd_selection(dd, offer):
    emit("dd_selection", present=bool(offer))


kbd = seat.get_keyboard()
kbd.dispatcher["keymap"] = kbd_keymap
kbd.dispatcher["enter"] = kbd_enter
kbd.dispatcher["leave"] = kbd_leave
kbd.dispatcher["key"] = kbd_key
kbd.dispatcher["modifiers"] = kbd_mods

ptr = seat.get_pointer()
if ptr is not None:
    ptr.dispatcher["enter"] = ptr_enter
    ptr.dispatcher["leave"] = ptr_leave
    ptr.dispatcher["motion"] = ptr_motion
    ptr.dispatcher["button"] = ptr_button
    ptr.dispatcher["axis"] = ptr_axis

if handles["ddm"] is not None:
    dd = handles["ddm"].get_data_device(seat)
    if dd is not None:
        dd.dispatcher["data_offer"] = dd_offer
        dd.dispatcher["selection"] = dd_selection

surf = handles["comp"].create_surface()
xdg_surf = handles["xdg"].get_xdg_surface(surf)
toplevel = xdg_surf.get_toplevel()
toplevel.set_title("selkies-wl-observer")

_tmp = tempfile.TemporaryFile(prefix="wlshm-buf")
fd = _tmp.fileno()
# A window sized to the compositor output: every injected pointer motion inside
# the captured output lands on this surface, so wl_pointer.motion streams.
W, H = 1280, 2160
stride = W * 4
size = stride * H
os.ftruncate(fd, size)
pool = handles["shm"].create_pool(fd, size)
buf = pool.create_buffer(0, W, H, stride, 0)
pool.destroy()


def xdg_configure(xsurf, serial):
    xsurf.ack_configure(serial)


got_configure = []

def xdg_configure2(xsurf, serial):
    xsurf.ack_configure(serial)
    got_configure.append(serial)

xdg_surf.dispatcher["configure"] = xdg_configure2
del xdg_configure
# Fullscreen so pointer motion lands on us regardless of compositor placement.
toplevel.set_fullscreen(handles["output"])
surf.commit()
for _ in range(30):
    if display.dispatch(block=False, queue=None) < 0:
        break
    display.roundtrip()
    if got_configure:
        break
    time.sleep(0.05)
surf.attach(buf, 0, 0)
surf.commit()
display.roundtrip()
emit("mapped")

end = time.time() + DURATION
while time.time() < end:
    if display.dispatch(block=True) < 0:
        break
emit("done")
