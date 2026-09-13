#!/usr/bin/env python3
"""The operator API: sessions, recording, screenshots, and the events they leave.

`/api/sessions` lists what the active transport reports and closes one page on
request; `/api/recording` drives pixelflux's MP4 recorder one recording at a
time and records each start and stop for the audit; `/api/screenshot` hands
over pixelflux's PNG of a display, the primary unnamed. A change is refused to
view-only credentials and accepted from the master token, and the listing each
transport builds carries the same keys.
"""
import asyncio
import os
import pathlib
import re
import sys
import tempfile
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.argv = ["selkies"]

from aiohttp import web  # noqa: E402
from aiohttp.test_utils import TestClient, TestServer  # noqa: E402
from selkies import audit  # noqa: E402
from selkies import selkies as S  # noqa: E402
from selkies.rtc import ClientType, RTCApp  # noqa: E402
from selkies.selkies import DataStreamingServer, client_permissions  # noqa: E402
from selkies.stream_server import CentralizedStreamServer, _uplink_session_state  # noqa: E402

MASTER, CTRL, VIEW = "unit-master-token", "unit-ctrl-token", "unit-view-token"
passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [operator-api] {label}  {detail}", flush=True)


class FakePixelflux:
    """The recorder and screenshot functions as pixelflux exposes them."""

    def __init__(self) -> None:
        self.active = self.last = None
        self.shots: list = []

    def recording_status(self):
        current = self.active or self.last
        return dict(current) if current else None

    def start_recording(self, path, settings=None):
        if self.active:
            raise RuntimeError("a recording is already active")
        self.active = {"active": True, "path": path, "frames": 0, "bytes": 0, "duration_s": 0.0}
        return dict(self.active)

    def stop_recording(self):
        if not self.active:
            raise RuntimeError("no recording is active")
        self.last = {**self.active, "active": False, "frames": 90, "bytes": 12345, "duration_s": 3.0}
        self.active = None
        return dict(self.last)

    def screenshot_png(self, display=0):
        self.shots.append(display)
        if display > 2:
            raise RuntimeError(f"Unknown display: {display}")
        return b"\x89PNG\r\n\x1a\n" + bytes([display])


class Service:
    def __init__(self) -> None:
        self.listed = [{"id": "abc", "transport": "fake", "role": "controller", "slot": None,
                        "display": "primary", "connected_at": "2026-01-01T00:00:00.000Z", "rtt_ms": 1.5}]
        self.closed: list = []

    async def sessions(self):
        return self.listed

    async def disconnect_session(self, session_id):
        self.closed.append(session_id)
        return session_id == "abc"


class Recorder:
    def __init__(self) -> None:
        self.events: list = []
        self._emit = audit.emit
        audit.emit = lambda event, **fields: self.events.append((event, fields))

    def close(self) -> None:
        audit.emit = self._emit


def settings(**over):
    base = dict(enable_basic_auth=(False,), basic_auth_user="user", basic_auth_password="secret",
                basic_auth_viewonly_password="", master_token=MASTER, subfolder="", allowed_origins="",
                wayland=(False, False))
    base.update(over)
    return SimpleNamespace(**base)


def bearer(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


async def route_cases(fake: FakePixelflux, root: str) -> None:
    S.user_tokens.clear()
    S.user_tokens.update({CTRL: {"role": "controller", "slot": 1}, VIEW: {"role": "viewer", "slot": None}})
    S.active_mk_token = None
    server = CentralizedStreamServer.__new__(CentralizedStreamServer)
    server.settings = settings()
    server.upload_dir = pathlib.Path(root)
    service = Service()
    server.services, server.current_mode = {"fake": service}, "fake"
    app = web.Application(middlewares=[server._auth_middleware])
    app["settings"] = server.settings
    app.add_routes([
        web.get("/api/sessions", server.handle_sessions),
        web.delete("/api/sessions/{id}", server.handle_session_delete),
        web.get("/api/recording", server.handle_recording),
        web.post("/api/recording", server.handle_recording),
        web.delete("/api/recording", server.handle_recording),
        web.get("/api/screenshot", server.handle_screenshot),
    ])
    client = TestClient(TestServer(app))
    await client.start_server()
    recorder = Recorder()
    try:
        r = await client.get("/api/sessions")
        check("no credential is challenged", r.status == 401, r.status)
        r = await client.get("/api/sessions", headers=bearer(CTRL))
        check("the listing is what the active transport reports",
              r.status == 200 and (await r.json()) == {"sessions": service.listed}, r.status)
        r = await client.get("/api/sessions", headers=bearer(VIEW))
        check("a viewer may read the listing", r.status == 200, r.status)
        r = await client.delete("/api/sessions/abc", headers=bearer(VIEW))
        check("a viewer may not disconnect anyone", r.status == 403 and not service.closed, r.status)
        r = await client.delete("/api/sessions/abc", headers=bearer(CTRL))
        check("a controller disconnects a page", r.status == 204 and service.closed == ["abc"], r.status)
        r = await client.delete("/api/sessions/abc", headers={**bearer(CTRL), "Origin": "http://elsewhere.example"})
        check("a browser's cross-site disconnect is refused", r.status == 403, r.status)
        r = await client.delete("/api/sessions/abc", headers={**bearer(MASTER), "Origin": "http://elsewhere.example"})
        check("the master token disconnects from anywhere", r.status == 204, r.status)
        r = await client.delete("/api/sessions/nope", headers=bearer(CTRL))
        check("an unknown session is not found", r.status == 404, r.status)
        server.current_mode = None
        r = await client.get("/api/sessions", headers=bearer(CTRL))
        check("no active transport lists nothing", (await r.json()) == {"sessions": []})
        server.current_mode = "fake"

        r = await client.get("/api/recording", headers=bearer(VIEW))
        check("before any recording the status is inactive",
              r.status == 200 and (await r.json()) == {"active": False}, r.status)
        r = await client.post("/api/recording", headers=bearer(VIEW))
        check("a viewer may not record", r.status == 403 and fake.active is None, r.status)
        r = await client.post("/api/recording", headers=bearer(CTRL))
        body = await r.json()
        check("a recording starts into the file-manager directory under a timestamped name",
              r.status == 200 and body["active"] and os.path.dirname(body["path"]) == root
              and re.fullmatch(r"recording-\d{8}T\d{6}Z\.mp4", os.path.basename(body["path"])), body)
        check("the start is recorded", recorder.events[-1] == ("recording.start", {"filename": body["path"]}),
              recorder.events[-1:])
        r = await client.post("/api/recording", headers=bearer(CTRL))
        check("a second start is a conflict", r.status == 409, r.status)
        r = await client.get("/api/recording", headers=bearer(CTRL))
        check("the status shows it active", (await r.json())["active"] is True)
        r = await client.delete("/api/recording", headers=bearer(CTRL))
        final = await r.json()
        check("stopping reports the finished file",
              r.status == 200 and final["active"] is False and final["frames"] == 90, final)
        check("the stop is recorded with the file's figures",
              recorder.events[-1] == ("recording.stop", {"filename": final["path"], "size_bytes": 12345,
                                                         "duration_s": 3.0, "frames": 90}), recorder.events[-1:])
        r = await client.delete("/api/recording", headers=bearer(CTRL))
        check("stopping nothing is a conflict", r.status == 409, r.status)
        r = await client.post("/api/recording", headers=bearer(CTRL), json={"path": "sub/take.mp4"})
        check("a relative path lands in the file-manager directory",
              (await r.json())["path"] == os.path.join(root, "sub", "take.mp4"))
        await client.delete("/api/recording", headers=bearer(CTRL))
        r = await client.post("/api/recording", headers=bearer(MASTER), json={"path": "/tmp/elsewhere.mp4"})
        check("an absolute path is used as given, and the master token may start one",
              (await r.json())["path"] == "/tmp/elsewhere.mp4")
        await client.delete("/api/recording", headers=bearer(MASTER))
        r = await client.post("/api/recording", headers={**bearer(CTRL), "Content-Type": "application/json"}, data=b"{")
        check("a body that is not JSON is refused", r.status == 400 and fake.active is None, r.status)

        r = await client.get("/api/screenshot", headers=bearer(VIEW))
        png = await r.read()
        check("a screenshot is the primary display's PNG",
              r.status == 200 and r.headers["Content-Type"] == "image/png" and png.startswith(b"\x89PNG")
              and fake.shots[-1] == 0, (r.status, fake.shots[-1:]))
        await client.get("/api/screenshot?display=display2", headers=bearer(CTRL))
        check("on X11 every display name is the one root", fake.shots[-1] == 0, fake.shots[-1:])
        server.settings.wayland = (True, False)
        await client.get("/api/screenshot?display=display2", headers=bearer(CTRL))
        await client.get("/api/screenshot?display=primary", headers=bearer(CTRL))
        check("on Wayland a display name is its output", fake.shots[-2:] == [2, 0], fake.shots[-2:])
        r = await client.get("/api/screenshot?display=display3", headers=bearer(CTRL))
        check("an output pixelflux does not know is not found", r.status == 404, r.status)
        delattr(FakePixelflux, "screenshot_png")
        r = await client.get("/api/screenshot", headers=bearer(CTRL))
        check("a pixelflux without screenshots says so", r.status == 501, r.status)
        sys.modules["pixelflux"] = None
        r = await client.post("/api/recording", headers=bearer(CTRL))
        check("no pixelflux at all says so", r.status == 501, r.status)
    finally:
        recorder.close()
        await client.close()
        S.user_tokens.clear()


class Socket:
    closed = False

    def __init__(self) -> None:
        self.closes: list = []
        self.pings: list = []

    async def ping(self, payload: bytes) -> None:
        self.pings.append(payload)
        state = _uplink_session_state(self)
        state["pending"].pop(payload, None)
        state["rtt_us"] = 12345
        state["seq"] += 1

    async def close(self, code=None, message=b"") -> None:
        self.closes.append((code, message))


class Peer:
    def __init__(self, state: str, rtts: list) -> None:
        self.connectionState = state
        self.rtts = rtts
        self.closed = False

    async def getStats(self):
        report = {f"r{i}": SimpleNamespace(type="remote-inbound-rtp", roundTripTime=rtt)
                  for i, rtt in enumerate(self.rtts)}
        report["out"] = SimpleNamespace(type="outbound-rtp")
        return report

    async def close(self) -> None:
        self.closed = True


async def transport_cases() -> None:
    recorder = Recorder()
    try:
        ws = DataStreamingServer.__new__(DataStreamingServer)
        controller, viewer, second = Socket(), Socket(), Socket()
        ws.clients = {controller, viewer, second}
        ws.display_clients = {"primary": {"ws": controller}, "display2": {"ws": second}}
        client_permissions.update({
            controller: {"id": "c1", "role": "controller", "slot": None, "connected_at": 1_000_000.0},
            viewer: {"id": "v1", "role": "viewer", "slot": 2, "connected_at": 1_000_001.0},
            second: {"id": "d2", "role": "controller", "slot": None, "connected_at": 1_000_002.0}})
        try:
            listed = {s["id"]: s for s in await ws.sessions()}
            check("websockets: every page is listed with its role, slot and display",
                  {k: (v["role"], v["slot"], v["display"], v["transport"]) for k, v in listed.items()} == {
                      "c1": ("controller", None, "primary", "websockets"),
                      "v1": ("viewer", 2, "primary", "websockets"),
                      "d2": ("controller", None, "display2", "websockets")}, listed)
            check("websockets: the round trip is measured on the spot and the time is RFC 3339",
                  all(s["rtt_ms"] == 12.3 for s in listed.values())
                  and listed["c1"]["connected_at"] == "1970-01-12T13:46:40.000Z" and len(controller.pings) == 1,
                  listed["c1"])
            check("websockets: a disconnect closes that socket alone",
                  await ws.disconnect_session("v1") and viewer.closes and not controller.closes, viewer.closes)
            check("websockets: an unknown id closes nothing", not await ws.disconnect_session("zz"))
            ws._audit_session_end(client_permissions[viewer])
            check("websockets: the end of a connection is recorded with its length",
                  recorder.events[-1][0] == "session.disconnect"
                  and recorder.events[-1][1]["role"] == "viewer" and recorder.events[-1][1]["slot"] == 2
                  and recorder.events[-1][1]["duration_s"] > 1_000_000, recorder.events[-1:])
            ws._audit_session_end({"role": "viewer"})
            check("websockets: a connection never recorded leaves no end",
                  recorder.events[-1][0] == "session.disconnect" and len(recorder.events) == 1)
        finally:
            for sock in (controller, viewer, second):
                client_permissions.pop(sock, None)

        rtc = RTCApp.__new__(RTCApp)
        rtc.peer_connections = {
            "p1": {"peer_conn": Peer("connected", [0.0123, 0.0101]), "client_type": ClientType.CONTROLLER,
                   "client_slot": None, "display_id": None, "connected_at": 1_000_000.0},
            "p2": {"peer_conn": Peer("connecting", [0.5]), "client_type": ClientType.VIEWER,
                   "client_slot": 3, "display_id": "display2", "connected_at": 1_000_001.0},
        }
        listed = {s["id"]: s for s in await rtc.sessions()}
        check("webrtc: every peer is listed with the same keys as websockets",
              set(listed["p1"]) == {"id", "transport", "role", "slot", "display", "connected_at", "rtt_ms"}
              and listed["p1"]["role"] == "controller" and listed["p2"]["role"] == "viewer"
              and listed["p2"]["slot"] == 3 and listed["p2"]["display"] == "display2", listed)
        check("webrtc: the round trip is the largest one the peer's reports carry, none before connection",
              listed["p1"]["rtt_ms"] == 12.3 and listed["p2"]["rtt_ms"] is None, listed)
        check("webrtc: a disconnect closes that peer",
              await rtc.disconnect_peer("p2") and rtc.peer_connections["p2"]["peer_conn"].closed
              and not rtc.peer_connections["p1"]["peer_conn"].closed)
        check("webrtc: an unknown peer closes nothing", not await rtc.disconnect_peer("zz"))
    finally:
        recorder.close()


async def main() -> None:
    fake = FakePixelflux()
    saved = sys.modules.get("pixelflux")
    sys.modules["pixelflux"] = fake  # type: ignore[assignment]
    root = tempfile.mkdtemp(prefix="selkies-operator-")
    try:
        await route_cases(fake, root)
        await transport_cases()
    finally:
        if saved is not None:
            sys.modules["pixelflux"] = saved
        else:
            sys.modules.pop("pixelflux", None)
    print(f"[operator-api] {passed}/{passed + failed} passed", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    asyncio.run(main())
