#!/usr/bin/env python3
"""Wayland observation client for e2e testing of the pixelflux compositor.

Connects to a compositor socket, maps a tiny xdg_toplevel surface, and listens
for seat keyboard/pointer events plus clipboard offers. Prints JSONL events on
stdout so the test driver can assert input/clipboard parity vs X11.
"""
import atexit
import json
import mmap
import os
import select
import signal
import struct
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
# Which of the compositor's outputs, in announcement order, the observer
# surface goes fullscreen on.
OUTPUT = int(os.environ.get("WLOBS_OUTPUT", "0"))
# Solid ARGB8888 colour (hex, e.g. ff2878dc) painted on the observer surface,
# so a captured frame carries a known picture; unset leaves the surface
# transparent and the compositor's own background shows through it.
FILL = int(os.environ.get("WLOBS_FILL", "0"), 16)
# A second colour the surface alternates with every WLOBS_BLINK_MS, so a
# damage-driven capture keeps receiving frames from an otherwise static screen.
FILL2 = int(os.environ.get("WLOBS_FILL2", "0"), 16)
BLINK_MS = int(os.environ.get("WLOBS_BLINK_MS", "0"))
# Keymap files handed to the driver; removed when this observer ends, so a
# suite that never reads them leaves nothing behind.
KEYMAP_FILES = []


def _remove_keymap_files() -> None:
    for path in KEYMAP_FILES:
        try:
            os.unlink(path)
        except OSError:
            pass


atexit.register(_remove_keymap_files)
# The driver stops the observer with SIGTERM; exit through atexit on it.
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))


def emit(kind: str, **kv) -> None:
    """Print one JSONL event line for the test driver to parse."""
    print(json.dumps({"kind": kind, **kv}), flush=True)


display = Display(SOCKET)
display.connect()
registry = display.get_registry()

handles = {
    "seat": None, "comp": None, "shm": None,
    "xdg": None, "ddm": None, "seat_iface": None, "seat_version": 1,
    "dd": None, "output": None, "outputs_seen": 0,
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
    elif iface == "wl_output":
        if handles["outputs_seen"] == OUTPUT:
            handles["output"] = reg.bind(name, WlOutput, version)
        handles["outputs_seen"] += 1


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
    """Save the keymap the seat hands out and report where it went: resolving
    a delivered keycode the way an application does needs that text together
    with the key and modifier events that follow it."""
    try:
        if size > 0:
            with mmap.mmap(fd, size, mmap.MAP_PRIVATE, mmap.PROT_READ) as m:
                text = bytes(m[:size]).rstrip(b"\0")
            fd_out, path = tempfile.mkstemp(prefix="wlobs-keymap-", suffix=".xkb")
            with os.fdopen(fd_out, "wb") as f:
                f.write(text)
            KEYMAP_FILES.append(path)
            emit("keymap", format=fmt, path=path, size=len(text))
    finally:
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


def offer_mimes(offer) -> dict:
    """Attach dispatchers that accumulate the offer's advertised mime types."""
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


# Devices are taken only for the capabilities the seat announces: a seat
# without a keyboard (a compositor started with no input devices) refuses
# get_keyboard with a protocol error.
caps = [0]
seat.dispatcher["capabilities"] = lambda _s, c: caps.__setitem__(0, int(c))
seat.dispatcher["name"] = lambda _s, _n: None
display.roundtrip()
kbd = seat.get_keyboard() if caps[0] & WlSeat.capability.keyboard else None
if kbd is not None:
    kbd.dispatcher["keymap"] = kbd_keymap
    kbd.dispatcher["enter"] = kbd_enter
    kbd.dispatcher["leave"] = kbd_leave
    kbd.dispatcher["key"] = kbd_key
    kbd.dispatcher["modifiers"] = kbd_mods

ptr = seat.get_pointer() if caps[0] & WlSeat.capability.pointer else None
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

got_configure = []
asked_size = [0, 0]


def xdg_configure2(xsurf, serial):
    xsurf.ack_configure(serial)
    got_configure.append(serial)


def toplevel_configure(_tl, width, height, _states):
    if width > 0 and height > 0:
        asked_size[0], asked_size[1] = width, height


xdg_surf.dispatcher["configure"] = xdg_configure2
toplevel.dispatcher["configure"] = toplevel_configure
# Fullscreen so pointer motion lands on us regardless of compositor placement;
# WLOBS_MAXIMIZE=1 asks for a maximized window instead, for a compositor whose
# fullscreen placement or size rules the observer cannot satisfy.
if os.environ.get("WLOBS_MAXIMIZE") == "1":
    toplevel.set_maximized()
else:
    toplevel.set_fullscreen(handles["output"])
surf.commit()
for _ in range(100):
    if display.dispatch(block=False, queue=None) < 0:
        break
    display.roundtrip()
    if got_configure:
        break
    time.sleep(0.05)
# The buffer takes the configured fullscreen size: a mismatched buffer is
# scaled or clipped by the compositor, which skews every surface-local
# coordinate this observer reports. The fallback covers a compositor that
# leaves the size to the client.
W, H = (asked_size[0], asked_size[1]) if asked_size[0] else (1280, 2160)
stride = W * 4
size = stride * H


def solid_buffer(fill):
    """A wl_shm buffer of the surface size painted `fill` (left zeroed when 0)."""
    tmp = tempfile.TemporaryFile(prefix="wlshm-buf")
    fd = tmp.fileno()
    os.ftruncate(fd, size)
    if fill:
        with mmap.mmap(fd, size) as m:
            m.write(struct.pack("<I", fill) * (W * H))
    pool = handles["shm"].create_pool(fd, size)
    buf = pool.create_buffer(0, W, H, stride, 0)
    pool.destroy()
    return tmp, buf


buffers = [solid_buffer(FILL)]
if BLINK_MS > 0:
    buffers.append(solid_buffer(FILL2))
surf.attach(buffers[0][1], 0, 0)
surf.commit()
display.roundtrip()
# `configured` says the size is the compositor's, not the fallback.
emit("mapped", w=W, h=H, configured=bool(asked_size[0]))

end = time.time() + DURATION
shown = 0
next_blink = time.time() + BLINK_MS / 1000.0
while time.time() < end:
    if BLINK_MS > 0:
        # Alternate the two buffers on the blink period, dispatching in between
        # so the seat's events are still reported as they arrive.
        display.flush()
        wait = max(0.0, next_blink - time.time())
        ready, _, _ = select.select([display.get_fd()], [], [], wait)
        if ready and display.dispatch(block=True) < 0:
            break
        if time.time() >= next_blink:
            shown ^= 1
            surf.attach(buffers[shown][1], 0, 0)
            surf.damage_buffer(0, 0, W, H)
            surf.commit()
            display.flush()
            next_blink += BLINK_MS / 1000.0
        continue
    if display.dispatch(block=True) < 0:
        break
emit("done")
