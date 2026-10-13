#!/usr/bin/env python3
"""A Wayland client that hands the compositor a virtual-keyboard keymap whose
size runs past its file, for e2e testing of the pixelflux compositor.

Usage: wl_vk_keymap.py <socket>   (VK_KEYMAP_SIZE: the size claimed, default 1 GiB)

The file holds a few bytes of keymap text; the request claims VK_KEYMAP_SIZE.
Prints JSONL: "sent" once the request went out and a roundtrip came back after
it, or "error" when the compositor offers no virtual keyboard or the
connection broke. pywayland carries no virtual-keyboard protocol, so its
bindings are generated at start from the protocol XML beside this file, as
wl_popup_client.py does for layer-shell.
"""
import json
import os
import sys
import tempfile

import pywayland.protocol
from pywayland.client import Display
from pywayland.protocol.wayland import WlSeat
from pywayland.scanner import Protocol

SOCKET = sys.argv[1] if len(sys.argv) > 1 else "wayland-1"
SIZE = int(os.environ.get("VK_KEYMAP_SIZE", str(1 << 30)))
XML = os.path.join(os.path.dirname(os.path.abspath(__file__)), "protocols", "virtual-keyboard-unstable-v1.xml")


def emit(kind: str, **kv) -> None:
    print(json.dumps({"kind": kind, **kv}), flush=True)


gen = tempfile.mkdtemp(prefix="wlgen-")
pkg = os.path.join(gen, "wlext")
os.makedirs(pkg)
open(os.path.join(pkg, "__init__.py"), "w").close()
base = list(pywayland.protocol.__path__)[0]
for name in os.listdir(base):
    if name.split(".")[0] == "wayland":
        os.symlink(os.path.join(base, name), os.path.join(pkg, name))
Protocol.parse_file(XML).output(pkg, {"wl_seat": "wayland",
                                      "zwp_virtual_keyboard_v1": "virtual_keyboard_unstable_v1",
                                      "zwp_virtual_keyboard_manager_v1": "virtual_keyboard_unstable_v1"})
sys.path.insert(0, gen)
from wlext.virtual_keyboard_unstable_v1 import ZwpVirtualKeyboardManagerV1  # noqa: E402

display = Display(SOCKET)
display.connect()
registry = display.get_registry()
found = {}


def on_global(reg, name, iface, version):
    if iface == "wl_seat" and "seat" not in found:
        found["seat"] = reg.bind(name, WlSeat, 1)
    elif iface == "zwp_virtual_keyboard_manager_v1":
        found["manager"] = reg.bind(name, ZwpVirtualKeyboardManagerV1, 1)


registry.dispatcher["global"] = on_global
display.roundtrip()
if "seat" not in found or "manager" not in found:
    emit("error", reason="no seat or virtual keyboard manager", found=sorted(found))
    sys.exit(2)
keyboard = found["manager"].create_virtual_keyboard(found["seat"])
keymap = tempfile.TemporaryFile()
keymap.write(b"xkb_keymap {};\n")
keymap.flush()
try:
    keyboard.keymap(1, keymap.fileno(), SIZE)
    display.roundtrip()
    display.roundtrip()
except Exception as e:  # noqa: BLE001
    emit("error", reason=repr(e))
    sys.exit(3)
emit("sent", size=SIZE)
