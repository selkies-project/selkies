#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Standalone Selkies JS-socket -> kernel uinput bridge.

Prefer enabling the integrated path instead:

    export SELKIES_ENABLE_UINPUT_BRIDGE=true

This helper is for external / legacy deployments that already expose
/tmp/selkies_js{0-3}.sock and need a real /dev/input gamepad for Steam.

Protocol (current Selkies main interposer):
  1. Server sends 1360-byte js_config_t
  2. Client sends 1-byte sizeof(unsigned long) architecture marker
  3. Server streams packed js_event structs (=IhbB)
"""

from __future__ import annotations

import logging
import os
import select
import socket
import struct
import sys
import time
from typing import Dict, List, Optional

from evdev import AbsInfo, UInput, ecodes as e

LOG = logging.getLogger("selkies-uinput-bridge")
logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")

C_CONFIG_SIZE = 1360
JS_EVENT_BUTTON = 0x01
JS_EVENT_AXIS = 0x02
JS_EVENT_INIT = 0x80
EVENT_FMT = "=IhbB"
EVENT_SIZE = struct.calcsize(EVENT_FMT)

ABS_MIN = -32767
ABS_MAX = 32767

BTN_CODES = [
    e.BTN_A, e.BTN_B, e.BTN_X, e.BTN_Y,
    e.BTN_TL, e.BTN_TR, e.BTN_SELECT, e.BTN_START,
    e.BTN_MODE, e.BTN_THUMBL, e.BTN_THUMBR,
]
AXIS_CODES = [
    e.ABS_X, e.ABS_Y, e.ABS_Z, e.ABS_RX,
    e.ABS_RY, e.ABS_RZ, e.ABS_HAT0X, e.ABS_HAT0Y,
]
HAT_AXES = {e.ABS_HAT0X, e.ABS_HAT0Y}

SOCKET_DIR = os.environ.get("SELKIES_JS_SOCKET_PATH", "/tmp")


def make_uinput(name: str = "Selkies Controller") -> UInput:
    stick = AbsInfo(value=0, min=ABS_MIN, max=ABS_MAX, fuzz=16, flat=128, resolution=0)
    hat = AbsInfo(value=0, min=-1, max=1, fuzz=0, flat=0, resolution=0)
    caps = {
        e.EV_KEY: list(BTN_CODES),
        e.EV_ABS: [(c, hat if c in HAT_AXES else stick) for c in AXIS_CODES],
    }
    ui = UInput(
        caps,
        name=name[:80],
        vendor=0x045E,
        product=0x028E,
        version=0x0114,
        bustype=0x03,
    )
    try:
        os.chmod(ui.device.path, 0o666)
    except OSError:
        pass
    return ui


def recv_exact(sock: socket.socket, n: int) -> Optional[bytes]:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def hat_value(raw: int) -> int:
    if raw > 0:
        return 1
    if raw < 0:
        return -1
    return 0


class PadBridge:
    def __init__(self, index: int, ui: UInput):
        self.index = index
        self.ui = ui
        self.socket_path = os.path.join(SOCKET_DIR, f"selkies_js{index}.sock")
        self.sock: Optional[socket.socket] = None
        self.btn_map: List[int] = list(BTN_CODES)
        self.axes_map: List[int] = list(AXIS_CODES)
        self._buf = bytearray()

    def connected(self) -> bool:
        return self.sock is not None

    def try_connect(self) -> bool:
        if self.sock is not None:
            return True
        if not os.path.exists(self.socket_path):
            return False
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.settimeout(3.0)
            sock.connect(self.socket_path)
            cfg = recv_exact(sock, C_CONFIG_SIZE)
            if not cfg:
                sock.close()
                return False
            # Architecture marker: sizeof(unsigned long) on this host
            sock.sendall(struct.pack("=B", struct.calcsize("L")))
            # Parse name / counts from packed config (name[255] + pad + fields)
            name = cfg[:255].split(b"\0", 1)[0].decode("utf-8", errors="replace") or "Selkies Controller"
            # After name(255)+pad(1): vendor,product,version,num_btns,num_axes as HHHHH
            try:
                _v, _p, _ver, num_btns, num_axes = struct.unpack_from("=HHHHH", cfg, 256)
                # btn_map starts after those 10 bytes at offset 266
                if 0 < num_btns <= len(BTN_CODES):
                    self.btn_map = list(struct.unpack_from(f"={num_btns}H", cfg, 266))
                if 0 < num_axes <= len(AXIS_CODES):
                    axes_off = 266 + 512 * 2  # INTERPOSER_MAX_BTNS * sizeof(uint16)
                    self.axes_map = list(struct.unpack_from(f"={num_axes}B", cfg, axes_off))
            except struct.error:
                pass
            sock.setblocking(False)
            self.sock = sock
            self._buf = bytearray()
            LOG.info("Attached to %s (%s) -> %s", self.socket_path, name, self.ui.device.path)
            return True
        except Exception as exc:
            LOG.debug("Connect %s: %s", self.socket_path, exc)
            try:
                sock.close()
            except Exception:
                pass
            self.sock = None
            return False

    def fileno(self) -> int:
        assert self.sock is not None
        return self.sock.fileno()

    def handle_events(self) -> None:
        assert self.sock is not None
        while True:
            try:
                data = self.sock.recv(EVENT_SIZE * 32)
            except BlockingIOError:
                return
            if not data:
                LOG.info("Socket closed for js%d (uinput kept alive)", self.index)
                self._close_sock_only()
                return
            self._buf.extend(data)
            while len(self._buf) >= EVENT_SIZE:
                ev = bytes(self._buf[:EVENT_SIZE])
                del self._buf[:EVENT_SIZE]
                self._emit(ev)

    def _emit(self, ev: bytes) -> None:
        _ts, value, typ, number = struct.unpack(EVENT_FMT, ev)
        typ &= ~JS_EVENT_INIT
        try:
            if typ == JS_EVENT_BUTTON:
                if number >= len(self.btn_map):
                    return
                self.ui.write(e.EV_KEY, self.btn_map[number], 1 if value else 0)
                self.ui.syn()
            elif typ == JS_EVENT_AXIS:
                if number >= len(self.axes_map):
                    return
                code = self.axes_map[number]
                # JS hat axes are full-range; map back to -1/0/1 for EV_ABS HAT
                if code in HAT_AXES:
                    out = hat_value(value)
                else:
                    out = int(value)
                self.ui.write(e.EV_ABS, code, out)
                self.ui.syn()
        except OSError as exc:
            LOG.warning("uinput write failed: %s", exc)

    def _close_sock_only(self) -> None:
        if self.sock is not None:
            try:
                self.sock.close()
            except Exception:
                pass
            self.sock = None
        self._buf = bytearray()


def main() -> int:
    LOG.info("Standalone Selkies uinput bridge (socket dir=%s)", SOCKET_DIR)
    if not os.path.exists("/dev/uinput"):
        LOG.error("/dev/uinput missing")
        return 1
    ui = make_uinput("Selkies Controller")
    LOG.info("Created persistent controller %s", ui.device.path)
    bridges: Dict[int, PadBridge] = {i: PadBridge(i, ui) for i in range(4)}
    try:
        while True:
            for b in bridges.values():
                if not b.connected():
                    b.try_connect()
            readers = [b for b in bridges.values() if b.connected()]
            if not readers:
                time.sleep(0.25)
                continue
            rlist, _, _ = select.select(readers, [], [], 0.5)
            for b in rlist:
                b.handle_events()
    finally:
        ui.close()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(0)
