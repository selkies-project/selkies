#!/usr/bin/env python3
"""The V4L2 interposer checks the staging layout its backend hands it.

The frame ring's geometry arrives over the socket with the memfd it describes.
A layout whose frames run past the end of that memfd fails the device's open()
with a log line, since a read of a frame beyond the end of the file faults the
application; a layout that fits is configured.

Serves both layouts from an in-process fake backend and opens the device
through the interposer with the libc-only probe; no pixelflux, no display.
"""
import os
import socket
import struct
import subprocess
import sys
import tempfile
import threading

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ADDON = os.path.join(ROOT, "addons", "v4l2-interposer")
TOOLS = os.path.join(ROOT, "tests", "tools")
INTERPOSER = os.path.join(ADDON, "selkies_v4l2_interposer.so")
PROBE = os.path.join(TOOLS, "v4l2probe")

MAGIC, VERSION = 0x434B5753, 1
MJPG = struct.unpack("<I", b"MJPG")[0]
DATA_OFFSET, CTRL_OFFSET, CTRL_STRIDE = 4096, 128, 64
SLOTS, SLOT_SIZE = 2, 1 << 20


def serve_once(path: str, memfd_size: int) -> threading.Thread:
    """Answer one interposer connection with the fixed layout over a file of `memfd_size`
    bytes, which the interposer maps as it would the backend's memfd."""
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(path)
    listener.listen(1)

    def run() -> None:
        conn, _ = listener.accept()
        staging = tempfile.TemporaryFile()
        fd = staging.fileno()
        os.ftruncate(fd, memfd_size)
        cfg = struct.pack("<14I8x", MAGIC, VERSION, 64, 48, MJPG, 30, 1, SLOTS, SLOT_SIZE,
                          DATA_OFFSET, CTRL_OFFSET, CTRL_STRIDE, 0, 4096)
        conn.sendmsg([cfg], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, struct.pack("i", fd))])
        conn.settimeout(3)
        try:
            conn.recv(1)
        except OSError:
            pass
        staging.close()
        conn.close()
        listener.close()

    t = threading.Thread(target=run, daemon=True)
    t.start()
    return t


def open_through_interposer(memfd_size: int) -> str:
    with tempfile.TemporaryDirectory() as sock_dir:
        server = serve_once(os.path.join(sock_dir, "selkies_webcam0.sock"), memfd_size)
        env = dict(os.environ, LD_PRELOAD=INTERPOSER, SELKIES_WEBCAM_SOCKET_PATH=sock_dir,
                   SELKIES_WEBCAM_SOURCE="socket", WEBCAM_LOG="1")
        p = subprocess.run([PROBE, "--timeout", "300", "/dev/video0", "1"], env=env,
                           capture_output=True, text=True, timeout=30)
        server.join(5)
        return p.stderr


def main() -> "H.Results":
    subprocess.run(["make", "-C", ADDON], check=True, stdout=subprocess.DEVNULL)
    subprocess.run(["make", "-C", TOOLS, "v4l2probe"], check=True, stdout=subprocess.DEVNULL)
    res = H.Results("v4l2-staging-layout")
    fits = DATA_OFFSET + SLOTS * SLOT_SIZE
    log = open_through_interposer(fits)
    res.check("a layout inside its memfd is configured", "configured 64x48" in log, log[-160:])
    log = open_through_interposer(2 * DATA_OFFSET)
    res.check("frames past the end of the memfd fail the open",
              "staging layout outside its memfd" in log and "configured 64x48" not in log, log[-160:])
    return res


if __name__ == "__main__":
    sys.exit(0 if main().summary() else 1)
