#!/usr/bin/env python3
"""The X11 desktop's density is settled before the server takes a page.

A home that outlives a restart brings the desktop up at the density its last
page gave it, and settling it merges resources and stamps the root, tens of
milliseconds into startup. A page connecting the moment the port opens, which
a browser retrying after a restart does, meets the settled density: its first
SETTINGS compares against what the desktop has, so a 1x page brings a desktop
left at 192 back to 96 instead of reading as no change. Both transports settle
it before they listen.

Usage: python3 tests/integration/test_dpi_startup_order.py [websockets|webrtc|all]
"""
import asyncio
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import websockets  # noqa: E402

MODES = ("websockets", "webrtc")
RESTORED = "for DPI 192."
LISTENING = "Selkies server running on"
PROCESSED = "settings applied for 'primary'"
APPLIED = "DPI changed from 192 to 96"


def x_env() -> dict:
    return {"DISPLAY": H.TEST_DISPLAY, "PATH": os.environ.get("PATH", "")}


def xft_dpi() -> Optional[int]:
    """Xft.dpi in the display's resource database, None when unset."""
    out = subprocess.run(["xrdb", "-query"], capture_output=True, text=True, env=x_env()).stdout
    for line in out.splitlines():
        if line.startswith("Xft.dpi:"):
            return int(line.split(":", 1)[1])
    return None


def leave_desktop_at(home: str, dpi: int) -> None:
    """The state a restart finds: the persisted resources, merged into the
    server as an image does at session start."""
    with open(os.path.join(home, ".Xresources"), "w") as f:
        f.write(f"Xft.dpi:   {dpi}\n")
    with open(os.path.join(home, ".xsettingsd"), "w") as f:
        f.write(f"Xft/DPI {dpi * 1024}\n")
    subprocess.run(["xrdb", "-merge", os.path.join(home, ".Xresources")], check=True, env=x_env())


def settings_payload() -> str:
    """The first SETTINGS of a page at device pixel ratio 1."""
    return "SETTINGS," + json.dumps({
        "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": False, "framerate": 60, "encoder": "h264enc", "scaling_dpi": 96})


async def first_page() -> Optional[float]:
    """Connect the moment the port accepts, send a 1x page's first SETTINGS,
    and hold the page open until they are in, as a browser would.

    Returns:
        Seconds from the accept to the SETTINGS going out, None when the port
        never opened.
    """
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        try:
            socket.create_connection(("127.0.0.1", H.PORT), timeout=0.05).close()
            break
        except OSError:
            await asyncio.sleep(0.001)
    else:
        return None
    accepted = time.monotonic()
    async with websockets.connect(f"ws://localhost:{H.PORT}/api/websockets", max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send(settings_payload())
        sent = time.monotonic() - accepted
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and PROCESSED not in H.server_log():
            await asyncio.sleep(0.2)
    return sent


def wait_for(pred, timeout: float = 20) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pred():
            return True
        time.sleep(0.25)
    return False


def run(mode: str, res: "H.Results", home: str) -> None:
    leave_desktop_at(home, 192)
    page: dict = {}
    worker = None
    if mode == "websockets":
        worker = threading.Thread(target=lambda: page.update(sent=asyncio.run(first_page())), daemon=True)
        worker.start()
    H.server_start(mode, extra_env={"HOME": home, "SELKIES_DEBUG": "true"})
    log = H.server_log()
    res.check(f"{mode}: the density left in the home is restored before the server listens",
              RESTORED in log and LISTENING in log and log.index(RESTORED) < log.index(LISTENING),
              [line for line in log.splitlines() if RESTORED in line or LISTENING in line])
    if worker is None:
        res.check(f"{mode}: the desktop keeps the density it was left at", xft_dpi() == 192, xft_dpi())
    else:
        worker.join(60)
        res.check(f"{mode}: a page connecting as the port opens has its first SETTINGS processed",
                  page.get("sent") is not None and PROCESSED in H.server_log(),
                  f"{page['sent'] * 1000:.0f} ms after the accept" if page.get("sent") is not None else "never")
        res.check(f"{mode}: its 1x SETTINGS brings a desktop left at 192 back to 96",
                  wait_for(lambda: xft_dpi() == 96), xft_dpi())
        res.check(f"{mode}: read as a change from 192", APPLIED in H.server_log(), APPLIED)
    H.server_stop()


def main() -> int:
    want = sys.argv[1] if len(sys.argv) > 1 else "all"
    res = H.Results("dpi-startup-order")
    xproc, H.TEST_DISPLAY = H.private_x_server(width=1280, height=720)
    home = tempfile.mkdtemp(prefix="selkies-dpi-home-")
    try:
        for mode in (MODES if want == "all" else (want,)):
            run(mode, res, home)
    finally:
        H.server_stop()
        shutil.rmtree(home, ignore_errors=True)
        H.stop_x_server(xproc, H.TEST_DISPLAY)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
