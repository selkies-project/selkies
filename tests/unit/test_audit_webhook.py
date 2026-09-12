#!/usr/bin/env python3
"""The audit webhook records every transfer and never costs a session anything.

Each clipboard payload the server sends or takes, each upload that lands or
fails, and each download served becomes one JSON POST to the operator's URL,
carrying metadata only. Events queue in emit order and one sender delivers
them over one keep-alive connection; a collector that rejects, stalls or is
down costs a session an enqueue and nothing more, is logged once per outage,
and loses only what overflows the queue. Without a URL nothing is created.
"""
import asyncio
import base64
import logging
import os
import re
import sys
import tempfile
import time
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.argv = ["selkies"]

from aiohttp import web  # noqa: E402
from aiohttp.test_utils import TestClient, TestServer  # noqa: E402
from selkies import audit  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402
from selkies.rtc import RTCApp  # noqa: E402
from selkies.selkies import SelkiesStreamingApp  # noqa: E402
from selkies.settings import SENSITIVE_SETTING_NAMES, build_client_settings_payload, settings  # noqa: E402
from selkies.stream_server import CentralizedStreamServer, TransferPacer  # noqa: E402

passed = failed = 0
RFC3339 = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$")


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [audit-webhook] {label}  {detail}", flush=True)


class _Capture(logging.Handler):
    def __init__(self) -> None:
        super().__init__(level=logging.DEBUG)
        self.lines: list = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append(f"{record.levelname} {record.getMessage()}")


class Channel:
    """A peer's open data channel, as the clipboard sender uses one."""

    readyState = "open"
    transport = None

    def __init__(self) -> None:
        self.sent: list = []

    def send(self, payload) -> None:
        self.sent.append(payload)


class Collector:
    """An audit collector that answers, rejects or stalls on command."""

    def __init__(self) -> None:
        self.mode = "ok"
        self.release = asyncio.Event()
        self.events: list = []
        self.headers: list = []
        self.peers: set = set()
        app = web.Application()
        app.router.add_post("/hook", self.hook)
        self.server = TestServer(app)

    async def hook(self, request: web.Request) -> web.Response:
        if self.mode == "stall":
            await self.release.wait()
        body = await request.json()
        if self.mode == "reject":
            return web.Response(status=500, text="no")
        self.events.append(body)
        self.headers.append(dict(request.headers))
        self.peers.add(request.transport.get_extra_info("peername"))
        return web.Response(status=204)

    async def __aenter__(self) -> "Collector":
        await self.server.start_server()
        self.url = f"http://127.0.0.1:{self.server.port}/hook"
        return self

    async def __aexit__(self, *exc) -> None:
        # The sender's keep-alive connection would otherwise hold the server's
        # graceful shutdown for its full timeout.
        await audit.close()
        self.release.set()
        await self.server.close()


async def delivered() -> None:
    await audit._queue.join()


async def reset(url: str, timeout: float = 2.0, token: str = "") -> None:
    await audit.close()
    settings.audit_webhook_url = url
    settings.audit_webhook_timeout = timeout
    settings.audit_webhook_token = token


async def delivery_cases(sink: _Capture) -> None:
    async with Collector() as collector:
        await reset(collector.url, token="s3cret")
        audit.emit("clipboard.send", mime_type="text/plain", size_bytes=5)
        audit.emit("clipboard.receive", mime_type="image/png", size_bytes=7, multipart=True)
        audit.emit("file.upload.end", filename="a/b.bin", size_bytes=9)
        audit.emit("file.upload.error", filename="c.bin", error="size mismatch")
        audit.emit("file.download", filename="d.bin", size_bytes=11)
        await delivered()
        got = collector.events
        check("every event arrives, in emit order",
              [e["event"] for e in got] == ["clipboard.send", "clipboard.receive", "file.upload.end",
                                            "file.upload.error", "file.download"], got)
        check("an event carries its metadata and nothing else",
              got[1] == {"event": "clipboard.receive", "ts": got[1].get("ts"), "mime_type": "image/png",
                         "size_bytes": 7, "multipart": True}, got[1])
        check("the timestamp is RFC 3339 UTC with milliseconds",
              all(RFC3339.match(e["ts"]) for e in got), [e["ts"] for e in got])
        check("timestamps come from emit time and keep order",
              [e["ts"] for e in got] == sorted(e["ts"] for e in got)
              and abs(time.time() - time.mktime(time.strptime(got[0]["ts"][:19], "%Y-%m-%dT%H:%M:%S"))
                      + time.timezone) < 5, got[0]["ts"])
        check("the token rides the Authorization header of every POST",
              all(h.get("Authorization") == "Bearer s3cret" for h in collector.headers), collector.headers[:1])
        check("the body is JSON", all(h.get("Content-Type") == "application/json" for h in collector.headers))
        check("one keep-alive connection carries every event", len(collector.peers) == 1, collector.peers)

        await reset(collector.url)
        audit.emit("file.download", filename="x", size_bytes=1)
        await delivered()
        check("no token, no Authorization header", "Authorization" not in collector.headers[-1])

        n = 1000
        start = time.perf_counter()
        for i in range(n):
            audit.emit("clipboard.send", mime_type="text/plain", size_bytes=i)
        per_event = (time.perf_counter() - start) / n * 1e6
        await delivered()
        check(f"an emit costs the loop microseconds ({per_event:.1f} us each) and loses nothing",
              per_event < 50 and len(collector.events) == 6 + n, len(collector.events))
        check("no warning during normal delivery", not [ln for ln in sink.lines if "WARNING" in ln], sink.lines[-2:])


async def outage_cases(sink: _Capture) -> None:
    async with Collector() as collector:
        await reset(collector.url, timeout=0.3)
        collector.mode = "reject"
        sink.lines.clear()
        for i in range(3):
            audit.emit("file.download", filename=f"r{i}", size_bytes=i)
        await delivered()
        warnings = [ln for ln in sink.lines if "WARNING" in ln]
        check("a rejecting collector is logged once, with the status",
              len(warnings) == 1 and "HTTP 500" in warnings[0], sink.lines)
        collector.mode = "ok"
        audit.emit("file.download", filename="back", size_bytes=1)
        await delivered()
        check("recovery is logged once and the next event arrives",
              sum("delivering again" in ln for ln in sink.lines) == 1
              and [e["filename"] for e in collector.events] == ["back"], sink.lines[-1:])

        collector.mode = "stall"
        sink.lines.clear()
        t0 = time.perf_counter()
        audit.emit("file.download", filename="slow", size_bytes=1)
        await delivered()
        took = time.perf_counter() - t0
        check("a stalled collector costs one timeout, then the sender moves on",
              0.25 < took < 1.0 and sum("WARNING" in ln for ln in sink.lines) == 1, f"{took:.2f}s {sink.lines}")

        sink.lines.clear()
        burst = audit.QUEUE_BOUND + 50
        t0 = time.perf_counter()
        for i in range(burst):
            audit.emit("clipboard.send", mime_type="text/plain", size_bytes=i)
        emit_time = time.perf_counter() - t0
        check("a burst past the bound never blocks and is logged once",
              emit_time < 0.1 and audit._queue.qsize() == audit.QUEUE_BOUND
              and sum("queue full" in ln for ln in sink.lines) == 1, f"{emit_time:.3f}s {sink.lines}")
        t0 = time.perf_counter()
        await audit.close()
        check("shutdown with a stalled collector waits one timeout at most",
              time.perf_counter() - t0 < 0.3 + 0.5 and audit._queue is None, f"{time.perf_counter() - t0:.2f}s")
        closed_port = collector.server.port

    sink.lines.clear()
    await reset(f"http://127.0.0.1:{closed_port}/hook", timeout=0.3)
    audit.emit("file.download", filename="down", size_bytes=1)
    await delivered()
    check("a collector that is down is logged once", sum("WARNING" in ln for ln in sink.lines) == 1, sink.lines)

    await reset("")
    audit.emit("file.download", filename="nowhere", size_bytes=1)
    check("without a URL nothing is queued or started", audit._queue is None and audit._sender is None)
    await audit.close()


async def hook_cases() -> None:
    with tempfile.TemporaryDirectory() as root:
      async with Collector() as collector:
          await reset(collector.url)
          server = CentralizedStreamServer.__new__(CentralizedStreamServer)
          server.settings = SimpleNamespace(file_transfers=["upload", "download"], file_manager_path=root)
          server.transfer_cap = TransferPacer(static_bps=0)
          server.upload_pacer = server.download_pacer = TransferPacer(adaptive=True)
          server.upload_dir = __import__("pathlib").Path(root).resolve()
          server._chunked_uploads = {}
          app = web.Application()
          app["settings"] = server.settings
          app.router.add_post("/api/upload", server.handle_upload)
          app.router.add_get("/api/files/{path:.*}", server.fancy_index_handler)
          client = TestClient(TestServer(app))
          await client.start_server()
          try:
              body = os.urandom(4096)
              r = await client.post("/api/upload", data=body, headers={"X-Upload-Path": "sub/plain.bin"})
              check("a plain upload lands", r.status == 200, r.status)
              r = await client.post("/api/upload", data=body[:2048], headers={
                  "X-Upload-Path": "sliced.bin", "X-Upload-Id": "t1", "X-Upload-Offset": "0", "X-Upload-Total": "4096"})
              r = await client.post("/api/upload", data=body[2048:], headers={
                  "X-Upload-Path": "sliced.bin", "X-Upload-Id": "t1", "X-Upload-Offset": "2048",
                  "X-Upload-Total": "4096", "X-Upload-Final": "1"})
              check("a chunked upload lands", r.status == 200, r.status)
              r = await client.post("/api/upload", data=body, headers={
                  "X-Upload-Path": "broken.bin", "X-Upload-Id": "t2", "X-Upload-Offset": "0",
                  "X-Upload-Total": "8192", "X-Upload-Final": "1"})
              check("a short chunked upload is refused", r.status == 400, r.status)
              r = await client.post("/api/upload", data=body, headers={"X-Upload-Path": "../escape.bin"})
              check("a traversal attempt is refused", r.status == 400, r.status)
              server.settings.file_transfers = ["download"]
              r = await client.post("/api/upload", data=body, headers={"X-Upload-Path": "policy.bin"})
              check("an upload under a download-only policy is refused", r.status == 403, r.status)
              server.settings.file_transfers = ["upload", "download"]
              r = await client.get("/api/files/sub/plain.bin")
              await r.read()
              check("a download is served", r.status == 200, r.status)
              r = await client.head("/api/files/sub/plain.bin")
              check("HEAD answers", r.status == 200, r.status)
              r = await client.get("/api/files/missing.bin")
              check("a missing file is 404", r.status == 404, r.status)
          finally:
              await client.close()
          await delivered()
          got = [(e["event"], e.get("filename"), e.get("size_bytes"), e.get("error")) for e in collector.events]
          check("uploads: one end per landed file with its path and size, one error per refusal, "
                "a refused path recorded as it was asked for",
                got[:5] == [("file.upload.end", "sub/plain.bin", 4096, None),
                            ("file.upload.end", "sliced.bin", 4096, None),
                            ("file.upload.error", "broken.bin", None, "size mismatch: received 4096, expected 8192"),
                            ("file.upload.error", "../escape.bin", None, "invalid upload path"),
                            ("file.upload.error", "policy.bin", None, "uploads disabled")], got)
          check("downloads: one event per GET of a file, none for HEAD or a miss",
                got[5:] == [("file.download", "sub/plain.bin", 4096, None)], got[5:])

          collector.events.clear()
          handler = WebRTCInput.__new__(WebRTCInput)
          handler.enable_clipboard = handler.enable_binary_clipboard = "true"
          handler._reset_multipart_clipboard()
          written: list = []

          async def write_clipboard(data, mime_type="text/plain", flavours=None):
              written.append((data, mime_type))
              return mime_type != "text/refused"

          handler.write_clipboard = write_clipboard
          b64 = lambda raw: base64.b64encode(raw).decode()
          await handler._dispatch_message("cw," + b64("hello".encode()))
          await handler._dispatch_message("cb,image/png," + b64(b"\x89PNG....."))
          await handler._dispatch_message("cb,text/refused," + b64(b"xx"))
          chunk = b"0123456789" * 20
          await handler._dispatch_message(f"cws,t9,{len(chunk) * 2}")
          await handler._dispatch_message("cwd,t9," + b64(chunk))
          await handler._dispatch_message("cwd,t9," + b64(chunk))
          await handler._dispatch_message("cwe,t9")
          handler.enable_clipboard = "out"
          await handler._dispatch_message("cw," + b64("blocked".encode()))
          await delivered()
          got = [(e["event"], e["mime_type"], e["size_bytes"], e["multipart"]) for e in collector.events]
          check("clipboard.receive: text, binary and multipart once the clipboard took them; a refused or "
                "disabled write records nothing",
                got == [("clipboard.receive", "text/plain", 5, False),
                        ("clipboard.receive", "image/png", 9, False),
                        ("clipboard.receive", "text/plain", 400, True)] and len(written) == 4, got)

          collector.events.clear()
          ws_app = SelkiesStreamingApp.__new__(SelkiesStreamingApp)
          fake = MagicMock(closed=False, send_str=AsyncMock(), send_bytes=AsyncMock())
          ws_app.data_streaming_server = SimpleNamespace(clients={fake}, enable_binary_clipboard=True)
          await ws_app.send_ws_clipboard_data("hello", "text/plain")
          await ws_app.send_ws_clipboard_data("", "text/plain", reply_to="cr")
          ws_app.data_streaming_server.enable_binary_clipboard = False
          await ws_app.send_ws_clipboard_data(b"\x89PNG", "image/png")
          rtc_app = RTCApp.__new__(RTCApp)
          rtc_app.peer_connections = {}
          await rtc_app.send_clipboard_data(b"\x89PNG..", "image/png")
          channel = Channel()
          rtc_app.peer_connections = {"peer": {"peer_conn": SimpleNamespace(connectionState="connected"),
                                               "data_channel": channel}}
          await rtc_app.send_clipboard_data(b"\x89PNG..", "image/png")
          await rtc_app.send_clipboard_data("", "text/plain", reply_to="cr")
          await delivered()
          got = [(e["event"], e["mime_type"], e["size_bytes"]) for e in collector.events]
          check("clipboard.send on both transports; a payload the server refuses to send, one with no "
                "peer to send it to, and an empty tagged reply that still goes out record nothing",
                got == [("clipboard.send", "text/plain", 5), ("clipboard.send", "image/png", 6)]
                and len(channel.sent) == 2, (got, len(channel.sent)))
          await audit.close()


def payload_case() -> None:
    payload = build_client_settings_payload()
    check("the webhook settings never reach a browser, the token as a secret",
          not [k for k in payload if k.startswith("audit_webhook")]
          and "audit_webhook_token" in SENSITIVE_SETTING_NAMES,
          [k for k in payload if k.startswith("audit_webhook")])


async def main_async() -> None:
    sink = _Capture()
    logging.getLogger("audit").addHandler(sink)
    try:
        await delivery_cases(sink)
        await outage_cases(sink)
        await hook_cases()
    finally:
        logging.getLogger("audit").removeHandler(sink)


def main() -> int:
    logging.basicConfig(level=logging.INFO)
    asyncio.run(main_async())
    payload_case()
    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
