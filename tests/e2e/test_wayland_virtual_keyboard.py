#!/usr/bin/env python3
"""A virtual-keyboard keymap the compositor cannot trust leaves it serving (Wayland).

A client of the session's compositor hands it a keymap whose size runs past
its file (tests/tools/wl_vk_keymap.py): the compositor ignores that keymap,
says so in the log, and keeps serving, so a fresh client still maps a window.

Usage: python3 tests/e2e/test_wayland_virtual_keyboard.py
"""
import importlib.util
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H


def main() -> bool:
    res = H.Results("wl-virtual-keyboard")
    if importlib.util.find_spec("pixelflux") is None:
        H.skip_suite("pixelflux is not installed")
    H.server_start(mode="websockets", wayland=True)
    obs = None
    try:
        sock = H.capture_socket()
        env = {**os.environ, "XDG_RUNTIME_DIR": H.RUNTIME_DIR}
        out = subprocess.run([H.PYTHON, os.path.join(H.TOOLS, "wl_vk_keymap.py"), sock],
                             capture_output=True, text=True, timeout=60, env=env)
        res.check("the client sends a keymap claiming 1 GiB of a 15-byte file",
                  '"kind": "sent"' in out.stdout, (out.stdout + out.stderr)[-300:])
        time.sleep(1.0)
        res.check("the compositor ignores it and says so",
                  "virtual-keyboard keymap of 1073741824 bytes" in H.server_log(), "")
        obs = H.WlObs(sock)
        res.check("and still maps a fresh client's window", obs.ready(15), obs.lines[-3:])
    finally:
        if obs is not None:
            obs.stop()
        H.server_stop()
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
