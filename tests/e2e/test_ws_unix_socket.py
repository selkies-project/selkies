#!/usr/bin/env python3
"""The websockets transport served from a Unix domain socket alone.

With ``unix_socket`` set the server binds that path instead of the TCP
addr/port pair, so everything — the static client, the API and the data
WebSocket that carries the stream — has to work over AF_UNIX. A stale socket
file left at the path by a dead server must be cleared at start (a live one
must not be), and the server must remove its own file on shutdown.

Checked two ways: a raw client on the socket itself (HTTP over AF_UNIX for
the API and the client files, a WebSocket over AF_UNIX that completes the
handshake, sends its settings and receives video frames), and a browser
through the TCP-to-Unix forwarder in tests/tools, standing in for the reverse
proxy such a deployment puts in front of the socket.
"""
import asyncio
import http.client
import json
import os
import socket
import subprocess
import sys
import time
from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
import websockets
from playwright.sync_api import sync_playwright

SOCK = os.path.join(H.WORKDIR, "selkies-ws.sock")
LOG = os.path.join(H.WORKDIR, "selkies-ws-unix.log")


class UnixHTTPConnection(http.client.HTTPConnection):
    """http.client over an AF_UNIX socket instead of TCP."""

    def __init__(self, path: str, timeout: float = 10) -> None:
        super().__init__("localhost", timeout=timeout)
        self.unix_path = path

    def connect(self) -> None:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(self.timeout)
        sock.connect(self.unix_path)
        self.sock = sock


def unix_get(path: str) -> tuple:
    """`(status, body)` of a GET over the Unix socket; status -1 when it is
    not answering."""
    conn = UnixHTTPConnection(SOCK)
    try:
        conn.request("GET", path)
        response = conn.getresponse()
        return response.status, response.read()
    except OSError:
        return -1, b""
    finally:
        conn.close()


def tcp_refused(port: int) -> bool:
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=2):
            return False
    except OSError:
        return True


def dead_socket_file(path: str) -> None:
    """Leave the socket inode of a listener that has since gone away."""
    if os.path.exists(path):
        os.unlink(path)
    stale = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stale.bind(path)
    stale.listen(1)
    stale.close()


def spawn_server(log: str) -> subprocess.Popen:
    """A server bound to SOCK, logging to `log`; not waited for."""
    env = {"PATH": os.environ.get("PATH", ""), "HOME": os.path.expanduser("~"),
           "DISPLAY": H.require_display(),
           "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", H.WORKDIR),
           "SELKIES_MODE": "websockets", "SELKIES_ENABLE_BASIC_AUTH": "false",
           "SELKIES_ENABLE_HTTPS": "false", "SELKIES_WEB_ROOT": H.CORE_DIST,
           "SELKIES_PORT": str(H.PORT), "SELKIES_UNIX_SOCKET": SOCK}
    with open(log, "w") as lf:
        lf.write("")
    return H.spawn([H.PYTHON, "-m", "selkies"], env=env, cwd=H.WORKDIR,
                   stdout=open(log, "a"), stderr=subprocess.STDOUT, start_new_session=True)


def start_server() -> subprocess.Popen:
    H.pulse_setup()
    proc = spawn_server(LOG)
    deadline = time.time() + 75
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"selkies died during startup; see {LOG}")
        if unix_get("/api/status")[0] == 200:
            return proc
        time.sleep(0.5)
    proc.terminate()
    raise RuntimeError(f"selkies did not come up on {SOCK}; see {LOG}")


def stop(proc: Optional[subprocess.Popen]) -> None:
    if proc is None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


async def stream_over_unix(seconds: float = 8.0) -> dict:
    """Complete the data-WebSocket handshake over the socket, send a primary
    display's settings and count the video frames that come back."""
    out = {"mode": None, "video_frames": 0, "error": None}
    settings = {"displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
                "is_manual_resolution_mode": False, "framerate": 60, "encoder": "h264enc",
                "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
                "scaling_dpi": 96, "displayPosition": "right"}
    try:
        async with websockets.unix_connect(SOCK, uri="ws://localhost/api/websockets", max_size=None) as ws:
            first = await asyncio.wait_for(ws.recv(), timeout=10)
            out["mode"] = first if isinstance(first, str) else repr(first[:20])
            await ws.send("SETTINGS," + json.dumps(settings))
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                try:
                    message = await asyncio.wait_for(ws.recv(), timeout=1.0)
                except asyncio.TimeoutError:
                    continue
                if isinstance(message, (bytes, bytearray)) and message and message[0] == 0x04:
                    out["video_frames"] += 1
                    if out["video_frames"] >= 30:
                        break
    except Exception as e:
        out["error"] = repr(e)[:120]
    return out


def main() -> bool:
    res = H.Results("ws-unix-socket")
    dead_socket_file(SOCK)
    server = proxy = None
    try:
        try:
            server = start_server()
        except RuntimeError as e:
            res.check("a dead socket file at the path is cleared and the server starts", False, e)
            return res.summary()
        res.check("a dead socket file at the path is cleared and the server starts", True)
        res.check("the server announces the unix endpoint",
                  C.wait_log(f"Selkies server running on unix://{SOCK}", timeout=10, log=LOG), "")
        res.check("no TCP listener on the port", tcp_refused(H.PORT), H.PORT)

        status, body = unix_get("/api/status")
        mode = json.loads(body or b"{}").get("current_mode") if status == 200 else None
        res.check("api/status answers over the socket", status == 200 and mode == "websockets", (status, mode))
        res.check("api/health answers over the socket", unix_get("/api/health")[0] == 200)
        status, body = unix_get("/")
        res.check("the client is served over the socket", status == 200 and b"<html" in body.lower(), status)

        got = asyncio.run(stream_over_unix())
        res.check("data WebSocket handshake completes over the socket",
                  got["mode"] is not None and got["mode"].startswith("MODE"), got)
        res.check("video frames flow over the socket", got["video_frames"] >= 30, got)
        res.check("capture started for the raw client",
                  C.wait_log("Capture started for 'primary'", timeout=10, log=LOG), "")

        proxy = H.spawn([H.PYTHON, os.path.join(H.TOOLS, "tcp2unix.py"), "127.0.0.1", str(H.PORT), SOCK],
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        deadline = time.time() + 10
        while time.time() < deadline and tcp_refused(H.PORT):
            time.sleep(0.25)
        with sync_playwright() as p:
            browser, page, console_errors, not_found = C.launch_chrome(p, mode="websockets")
            try:
                info = C.wait_ws_video(page, timeout=30)
                res.check("browser through the forwarder: video canvas painted", info is not None, info)
                deadline = time.time() + 10
                frames = 0
                while time.time() < deadline and frames < 24:
                    frames = page.evaluate("window.__wsFrames") or 0
                    time.sleep(0.5)
                res.check("browser through the forwarder: frames flowing", frames >= 24, frames)
                real_errors, _ = C.benign_console(console_errors, not_found)
                res.check("browser console clean", not real_errors, str(real_errors[:2]))
            finally:
                browser.close()

        # A live socket is never unlinked from under its owner: a second server
        # on the same path refuses to start, and the first keeps serving.
        second_log = LOG + ".second"
        second = spawn_server(second_log)
        try:
            second.wait(timeout=60)
        except subprocess.TimeoutExpired:
            stop(second)
        res.check("a second server on the live socket refuses to start",
                  second.returncode not in (None, 0)
                  and "already listening" in H.server_log(second_log), second.returncode)
        res.check("the first server still answers after the refused start",
                  unix_get("/api/status")[0] == 200)
        stop(server)
        server = None
        res.check("the socket file is removed on shutdown", not os.path.exists(SOCK), SOCK)
    finally:
        stop(proxy)
        stop(server)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
