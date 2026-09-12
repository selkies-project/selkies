#!/usr/bin/env python3
"""A live session's transfers, as an audit collector sees them.

Every channel is driven the way a client drives it -- an upload and a download
over HTTP, a paste into the session and a copy out of it over the WebSocket --
against a real collector, and each one has to arrive as one event carrying
metadata and no content. The collector is then stalled to prove what the
queued sender buys: a transfer runs at its own speed whatever the collector
does with the POST.
Runs against its own X server and its own selkies.
Usage: python3 tests/integration/test_audit_webhook.py
"""
import asyncio
import base64
import http.client
import json
import os
import sys
import tempfile
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(REPO, "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
import websockets  # noqa: E402

TOKEN = "audit-integration-token"
PASTED = b"pasted into the session"
STREAM = {"displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
          "manual_resolution": False, "framerate": 60, "video_crf": 25,
          "video_bitrate": 6000, "audio_bitrate": 128000, "scaling_dpi": 96}
COPIED = b"copied out of the session"
TIMEOUT = 1.0


class Collector:
    """The operator's collector: records the POSTs, and stalls them on demand."""

    def __init__(self) -> None:
        self.events: list = []
        self.headers: list = []
        self.stall = threading.Event()
        collector = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                while collector.stall.is_set():
                    time.sleep(0.05)
                collector.events.append(json.loads(body))
                collector.headers.append(dict(self.headers))
                self.send_response(204)
                self.end_headers()

            def log_message(self, *_: object) -> None:
                pass

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.httpd.server_port}/audit"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def wait(self, count: int, timeout: float = 15) -> list:
        """The events received once `count` of them have arrived, or time is up."""
        end = time.time() + timeout
        while time.time() < end and len(self.events) < count:
            time.sleep(0.05)
        return list(self.events)

    def stop(self) -> None:
        self.stall.clear()
        self.httpd.shutdown()


def request(method: str, path: str, headers: dict = None, body: bytes = None) -> tuple:
    conn = http.client.HTTPConnection("localhost", H.PORT, timeout=60)
    try:
        conn.request(method, path, body=body, headers=dict(headers or {}))
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def upload(name: str, data: bytes) -> int:
    status, _ = request("POST", "/api/upload", headers={
        "Content-Type": "application/octet-stream",
        "X-Upload-Path": urllib.parse.quote(name)}, body=data)
    return status


async def clipboard_round_trip() -> None:
    """Paste into the session, then ask for the session's own clipboard back."""
    ws = await websockets.connect(f"ws://localhost:{H.PORT}/api/websockets", max_size=None)
    try:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send("cw," + base64.b64encode(PASTED).decode())
        await asyncio.sleep(1.5)
        ext, stop = H.x_own_clipboard(COPIED)
        try:
            await ws.send("cr")
            await asyncio.sleep(2.5)
        finally:
            stop["flag"] = True
    finally:
        await ws.close()


async def frames_while_pasting(seconds: float) -> int:
    """Frames received over `seconds` while pasting into the session throughout.

    Each paste is one audit event, so a run against a collector that never
    answers is what a POST on the event loop would show up in.
    """
    ws = await websockets.connect(f"ws://localhost:{H.PORT}/api/websockets", max_size=None)
    try:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send("SETTINGS," + json.dumps(STREAM))
        seen, last = 0, -1
        end = time.monotonic() + seconds
        paste_at = time.monotonic()
        while time.monotonic() < end:
            try:
                message = await asyncio.wait_for(ws.recv(), timeout=0.2)
            except asyncio.TimeoutError:
                message = None
            if isinstance(message, (bytes, bytearray)) and len(message) >= 4 and message[0] in (0x03, 0x04):
                frame_id = (message[2] << 8) | message[3]
                if frame_id != last:
                    last, seen = frame_id, seen + 1
                    await ws.send(f"CLIENT_FRAME_ACK {frame_id} 0")
            if time.monotonic() >= paste_at:
                await ws.send("cw," + base64.b64encode(PASTED).decode())
                paste_at = time.monotonic() + 0.2
        return seen
    finally:
        await ws.close()


def main() -> H.Results:
    res = H.Results("audit-webhook")
    collector = Collector()
    xproc = None
    files = tempfile.mkdtemp(prefix="audit-files-")
    if not H.TEST_DISPLAY:
        xproc, H.TEST_DISPLAY = H.private_x_server()
    try:
        H.server_start(mode="websockets", wayland=False, extra_env={
            # The server imports this worktree rather than the editable install.
            "PYTHONPATH": os.path.join(REPO, "src"),
            "SELKIES_AUDIT_WEBHOOK_URL": collector.url,
            "SELKIES_AUDIT_WEBHOOK_TOKEN": TOKEN,
            "SELKIES_AUDIT_WEBHOOK_TIMEOUT": str(TIMEOUT),
            "SELKIES_FILE_MANAGER_PATH": files,
        })
        body = os.urandom(64 * 1024)
        res.check("an upload lands", upload("audit/landed.bin", body) == 200)
        res.check("a traversal attempt is refused", upload("../escaped.bin", body) == 400)
        status, served = request("GET", "/api/files/audit/landed.bin")
        res.check("a download is served", status == 200 and served == body, status)
        asyncio.run(clipboard_round_trip())

        got = collector.wait(6)
        kinds = [e["event"] for e in got]
        res.check("every channel is recorded",
                  set(kinds) == {"clipboard.receive", "clipboard.send", "file.download",
                                 "file.upload.end", "file.upload.error"}, kinds)
        by_event = {e["event"]: e for e in got}
        res.check("the upload carries its path and size",
                  (by_event.get("file.upload.end", {}).get("filename"),
                   by_event.get("file.upload.end", {}).get("size_bytes")) == ("audit/landed.bin", len(body)),
                  by_event.get("file.upload.end"))
        res.check("the refusal carries the path as it was asked for",
                  by_event.get("file.upload.error", {}).get("filename") == "../escaped.bin",
                  by_event.get("file.upload.error"))
        res.check("the download carries its path and size",
                  (by_event.get("file.download", {}).get("filename"),
                   by_event.get("file.download", {}).get("size_bytes")) == ("audit/landed.bin", len(body)),
                  by_event.get("file.download"))
        res.check("the paste is recorded with its size",
                  by_event.get("clipboard.receive", {}).get("size_bytes") == len(PASTED),
                  by_event.get("clipboard.receive"))
        res.check("the paste is not recorded as content when it is announced back to the clients",
                  not any("pasted" in json.dumps(e) or "copied" in json.dumps(e) for e in got), got[:1])
        # The paste is not announced back -- the monitor knows the server wrote it --
        # so the two sends are the announcement of the copy and the reply to the request.
        sends = [e["size_bytes"] for e in got if e["event"] == "clipboard.send"]
        res.check("the copy leaving the session is recorded each time it is sent",
                  sends == [len(COPIED), len(COPIED)], sends)
        res.check("every POST carries the configured token",
                  all(h.get("Authorization") == f"Bearer {TOKEN}" for h in collector.headers),
                  collector.headers[:1])

        # What the queue buys: the collector holds every POST open, and the
        # transfers still run at the speed they run at when it answers.
        baseline = time.monotonic()
        res.check("a second upload lands", upload("audit/second.bin", body) == 200)
        baseline = time.monotonic() - baseline
        collector.stall.set()
        stalled = time.monotonic()
        res.check("an upload lands while the collector stalls", upload("audit/stalled.bin", body) == 200)
        status, _ = request("GET", "/api/files/audit/stalled.bin")
        stalled = time.monotonic() - stalled
        res.check("a download is served while the collector stalls", status == 200, status)
        res.check("a stalled collector does not pace the transfers",
                  stalled < max(baseline * 4, 1.0), f"{stalled:.2f}s stalled vs {baseline:.2f}s answering")
        collector.stall.clear()

        # The stream's own rate, with the channel off and then on against a
        # collector that never answers: an event that reached the loop would
        # cost frames here.
        churn = C.Churn()
        churn.start()
        try:
            H.server_start(mode="websockets", wayland=False, extra_env={
                "PYTHONPATH": os.path.join(REPO, "src"), "SELKIES_FILE_MANAGER_PATH": files})
            quiet = asyncio.run(frames_while_pasting(8))
            collector.stall.set()
            H.server_start(mode="websockets", wayland=False, extra_env={
                "PYTHONPATH": os.path.join(REPO, "src"),
                "SELKIES_AUDIT_WEBHOOK_URL": collector.url,
                "SELKIES_AUDIT_WEBHOOK_TIMEOUT": str(TIMEOUT),
                "SELKIES_FILE_MANAGER_PATH": files})
            audited = asyncio.run(frames_while_pasting(8))
        finally:
            churn.stop()
            collector.stall.clear()
        res.check("the audit channel costs the stream no frames",
                  quiet > 100 and audited >= quiet * 0.9,
                  f"{audited} frames audited against a stalled collector, {quiet} with the channel off")
    finally:
        H.server_stop()
        collector.stop()
        if xproc is not None:
            H.stop_x_server(xproc, H.TEST_DISPLAY)
    return res


if __name__ == "__main__":
    sys.exit(0 if main().summary() else 1)
