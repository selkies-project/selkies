#!/usr/bin/env python3
"""Printed documents reach the browser through the spool.

A document that lands whole in the print spool is announced once, whatever
way it landed; `/api/print/<name>` serves it, records it for the audit, and
takes it out of the spool, refusing a viewer, a disabled policy, and any name
that is not a document in the spool; and each transport announces to the
primary controller pages alone.
"""
import asyncio
import json
import os
import shutil
import sys
import tempfile
import time
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.argv = ["selkies"]

from aiohttp import web  # noqa: E402
from aiohttp.test_utils import TestClient, TestServer  # noqa: E402
from selkies import audit, printing  # noqa: E402
from selkies.webrtc_engine import ClientType, RTCApp  # noqa: E402
from selkies.websockets_mode import DataStreamingServer, client_permissions  # noqa: E402
from selkies.stream_server import CentralizedStreamServer  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [printing] {label}  {detail}", flush=True)


def name_cases() -> None:
    check("a PDF name passes", printing.document_name("Report (2).pdf") == "Report (2).pdf")
    check("case of the suffix does not matter", printing.document_name("scan.PDF") == "scan.PDF")
    for bad in ("../x.pdf", "sub/x.pdf", ".hidden.pdf", ".3.part", "notes.txt", "x\x00.pdf", ""):
        check(f"{bad!r} is not a document", printing.document_name(bad) is None)


def pending_cases(spool: str) -> None:
    for name, delay in (("second.pdf", 0.02), ("first.pdf", 0.0), ("skip.txt", 0.0), (".part", 0.0)):
        open(os.path.join(spool, name), "wb").write(b"%PDF" * 3)
        os.utime(os.path.join(spool, name), (time.time() + delay, time.time() + delay))
    check("pending lists the documents oldest first, nothing else",
          printing.pending(spool) == [("first.pdf", 12), ("second.pdf", 12)], printing.pending(spool))
    check("a missing spool is empty", printing.pending(os.path.join(spool, "none")) == [])
    for name in os.listdir(spool):
        os.unlink(os.path.join(spool, name))


async def watcher_cases(spool: str, elsewhere: str) -> None:
    seen: list = []

    async def on_document(name: str, size: int) -> None:
        seen.append((name, size))

    watcher = printing.SpoolWatcher(spool, asyncio.get_running_loop(), on_document)
    watcher.start()
    await asyncio.sleep(0.3)

    async def settled(count: int) -> list:
        for _ in range(60):
            if len(seen) >= count:
                break
            await asyncio.sleep(0.05)
        await asyncio.sleep(printing.SETTLE_SECONDS + 0.3)
        return list(seen)

    part = os.path.join(spool, ".7.part")
    open(part, "wb").write(b"%PDF" * 100)
    os.replace(part, os.path.join(spool, "renamed.pdf"))
    check("a document renamed into place is announced once, whole",
          await settled(1) == [("renamed.pdf", 400)], seen)

    seen.clear()
    with open(os.path.join(spool, "written.pdf"), "wb") as f:
        f.write(b"%PDF")
        f.flush()
        await asyncio.sleep(0.2)
        f.write(b"x" * 1000)
    check("a document written in place is announced once, at its final size",
          await settled(1) == [("written.pdf", 1004)], seen)

    seen.clear()
    open(os.path.join(elsewhere, "moved.pdf"), "wb").write(b"%PDF" * 300)
    os.replace(os.path.join(elsewhere, "moved.pdf"), os.path.join(spool, "moved.pdf"))
    check("a document moved in from another directory is announced once it has settled",
          await settled(1) == [("moved.pdf", 1200)], seen)

    seen.clear()
    open(os.path.join(spool, "notes.txt"), "wb").write(b"text")
    open(os.path.join(spool, ".hidden.pdf"), "wb").write(b"%PDF")
    check("other files and hidden ones are not announced", await settled(1) == [], seen)
    watcher.stop()
    for name in os.listdir(spool):
        os.unlink(os.path.join(spool, name))


class Recorder:
    """Keeps the audit events the route emits."""

    def __init__(self) -> None:
        self.events: list = []
        self._emit = audit.emit
        audit.emit = lambda event, **fields: self.events.append((event, fields))

    def close(self) -> None:
        audit.emit = self._emit


async def settled(condition) -> bool:
    """Whether `condition` holds within a moment: the audit event and the
    removal follow the body, which the client may finish reading first."""
    for _ in range(100):
        if condition():
            return True
        await asyncio.sleep(0.01)
    return condition()


async def route_cases(spool: str) -> None:
    server = CentralizedStreamServer.__new__(CentralizedStreamServer)
    server.settings = SimpleNamespace(printing_enabled=(True, False))
    server.print_spool = __import__("pathlib").Path(spool).resolve()
    server.print_watcher = object()
    server.services, server.current_mode = {}, None

    @web.middleware
    async def role(request, handler):
        if request.headers.get("X-Test-Viewer"):
            request["auth_role_ceiling"] = "viewer"
        return await handler(request)

    app = web.Application(middlewares=[role])
    app.router.add_get("/api/print/{name}", server.handle_print_document)
    client = TestClient(TestServer(app))
    await client.start_server()
    recorder = Recorder()
    try:
        body = b"%PDF-1.4 " + os.urandom(2048)
        open(os.path.join(spool, "Report.pdf"), "wb").write(body)
        r = await client.head("/api/print/Report.pdf")
        check("HEAD answers without taking the document",
              r.status == 200 and os.path.exists(os.path.join(spool, "Report.pdf")) and not recorder.events, r.status)
        r = await client.get("/api/print/Report.pdf")
        got = await r.read()
        check("the document is served inline as a PDF",
              r.status == 200 and got == body and r.headers.get("Content-Type") == "application/pdf"
              and r.headers.get("Content-Disposition") == "inline", (r.status, r.headers.get("Content-Type")))
        check("the audit records it as print.document with the bytes served",
              await settled(lambda: recorder.events == [
                  ("print.document", {"filename": "Report.pdf", "size_bytes": len(body), "partial": False})]),
              recorder.events)
        check("a document that went out whole leaves the spool",
              await settled(lambda: not os.path.exists(os.path.join(spool, "Report.pdf"))))
        r = await client.get("/api/print/Report.pdf")
        check("and is gone the second time", r.status == 404, r.status)
        check("pending is empty once taken", server.pending_print_documents() == [])

        open(os.path.join(spool, "Range.pdf"), "wb").write(body)
        r = await client.get("/api/print/Range.pdf", headers={"Range": "bytes=0-99"})
        await r.read()
        check("a range is served and recorded as partial, and the document stays",
              r.status == 206 and await settled(lambda: recorder.events[-1] == (
                  "print.document", {"filename": "Range.pdf", "size_bytes": 100, "partial": True}))
              and os.path.exists(os.path.join(spool, "Range.pdf")), (r.status, recorder.events[-1:]))
        check("pending names it", server.pending_print_documents() == [("Range.pdf", len(body))])

        r = await client.get("/api/print/Range.pdf", headers={"X-Test-Viewer": "1"})
        check("view-only credentials are refused", r.status == 403, r.status)
        for bad in ("..%2FRange.pdf", "notes.txt", ".hidden.pdf", "missing.pdf"):
            r = await client.get(f"/api/print/{bad}")
            check(f"{bad} is not served", r.status == 404, r.status)
        server.settings.printing_enabled = (False, True)
        r = await client.get("/api/print/Range.pdf")
        check("a disabled policy refuses every document", r.status == 403, r.status)
        server.settings.printing_enabled = (True, False)
        server.print_watcher = None
        check("without a watcher nothing is pending", server.pending_print_documents() == [])
    finally:
        recorder.close()
        await client.close()
        for name in os.listdir(spool):
            os.unlink(os.path.join(spool, name))


class Socket:
    """A client socket as the fan-out sends to it."""

    closed = False

    def __init__(self) -> None:
        self.sent: list = []

    async def send_str(self, message: str) -> None:
        self.sent.append(json.loads(message))


class Channel:
    readyState = "open"

    def __init__(self) -> None:
        self.sent: list = []

    def send(self, payload: str) -> None:
        self.sent.append(json.loads(payload))


async def transport_cases() -> None:
    ws = DataStreamingServer.__new__(DataStreamingServer)
    controller, viewer, second = Socket(), Socket(), Socket()
    ws.clients = {controller, viewer, second}
    ws.display_clients = {"primary": {"ws": controller}, "display2": {"ws": second}}
    client_permissions.update({controller: {"role": "controller"}, viewer: {"role": "viewer"},
                               second: {"role": "controller"}})
    try:
        await ws.announce_print_document("Report.pdf", 12)
        check("websockets: the primary controller page is told, the viewer and the second display are not",
              controller.sent == [{"type": "print_document", "name": "Report.pdf", "size_bytes": 12}]
              and not viewer.sent and not second.sent, (controller.sent, viewer.sent, second.sent))
    finally:
        for sock in (controller, viewer, second):
            client_permissions.pop(sock, None)

    rtc = RTCApp.__new__(RTCApp)
    peers = {}
    for pid, ctype, did in (("c", ClientType.CONTROLLER, None), ("v", ClientType.VIEWER, None),
                            ("d2", ClientType.CONTROLLER, "display2")):
        peers[pid] = {"client_type": ctype, "display_id": did, "data_channel": Channel(),
                      "peer_conn": SimpleNamespace(connectionState="connected")}
    rtc.peer_connections = peers
    rtc.send_print_document("Report.pdf", 12)
    check("webrtc: the primary controller peer is told, the viewer and the second display are not",
          peers["c"]["data_channel"].sent == [{"type": "print_document", "data": {"name": "Report.pdf", "size_bytes": 12}}]
          and not peers["v"]["data_channel"].sent and not peers["d2"]["data_channel"].sent,
          [p["data_channel"].sent for p in peers.values()])
    one = Channel()
    rtc.send_print_document("Late.pdf", 3, one)
    check("webrtc: one channel can be told alone, for a page that connects with documents pending",
          one.sent == [{"type": "print_document", "data": {"name": "Late.pdf", "size_bytes": 3}}], one.sent)


async def main() -> None:
    root = tempfile.mkdtemp(prefix="selkies-printing-")
    spool, elsewhere = os.path.join(root, "spool"), os.path.join(root, "elsewhere")
    os.makedirs(spool)
    os.makedirs(elsewhere)
    try:
        name_cases()
        pending_cases(spool)
        await watcher_cases(spool, elsewhere)
        await route_cases(spool)
        await transport_cases()
    finally:
        shutil.rmtree(root, ignore_errors=True)
    print(f"[printing] {passed}/{passed + failed} passed", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    asyncio.run(main())
