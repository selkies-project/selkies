#!/usr/bin/env python3
"""Control-plane guards that do not need a display.

The access log must not carry the query string (the secure-mode session token
rides the data WebSocket URL as a query parameter), the mode-switch POST must be
held to the same Origin rule as the WebSocket upgrades unless the Bearer master
token authenticates it, a secure-mode session token is looked up in constant
time, and the MK_ACCESS verdict a websockets client is told on connect matches
the one WebRTC pushes at channel open (a viewer additionally needs collab).
"""
import asyncio
import base64
import json
import logging
import os
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from aiohttp import web  # noqa: E402
from aiohttp.test_utils import TestClient, TestServer, make_mocked_request  # noqa: E402

from selkies.stream_server import CentralizedStreamServer, PathOnlyAccessLogger  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [control-plane] {label}  {detail}", flush=True)


class _Capture(logging.Handler):
    """Collects the formatted lines a logger emits."""

    def __init__(self) -> None:
        super().__init__(level=logging.DEBUG)
        self.lines: list = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append(self.format(record))


class _Settings:
    """The settings fields the auth middleware reads, with per-case overrides."""

    def __init__(self, **over) -> None:
        self.enable_basic_auth = (True,)
        self.basic_auth_user = "user"
        self.basic_auth_password = "secret"
        self.basic_auth_viewonly_password = ""
        self.master_token = ""
        self.subfolder = ""
        self.allowed_origins = ""
        for key, value in over.items():
            setattr(self, key, value)


def _basic(user: str, password: str) -> str:
    return "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()


async def access_log_cases() -> None:
    """The logger's own line, and the line the server emits through it."""
    log = logging.getLogger("selkies-test-access")
    log.setLevel(logging.INFO)
    log.propagate = False
    sink = _Capture()
    log.addHandler(sink)

    access = PathOnlyAccessLogger(log, "")
    request = make_mocked_request(
        "GET", "/api/websockets?token=s3cret-tok&role=viewer",
        headers={"User-Agent": "probe/1", "Referer": "http://example.test/?token=ref-tok"})
    response = web.Response(status=200, text="ok")
    access.log(request, response, 0.01)
    line = sink.lines[-1] if sink.lines else ""
    check("access line carries the request path", '"GET /api/websockets HTTP/1.1"' in line, line)
    check("access line drops the query string", "s3cret-tok" not in line and "role=viewer" not in line, line)
    check("access line keeps the status and agent", " 200 " in line and "probe/1" in line, line)

    sink.lines.clear()
    app = web.Application()
    app.router.add_get("/api/status", lambda request: web.Response(text="OK"))
    # The runner takes the logger class the way start_server hands it to AppRunner.
    server = TestServer(app)
    await server.start_server(access_log_class=PathOnlyAccessLogger, access_log=log)
    client = TestClient(server)
    await client.start_server()
    try:
        response = await client.get("/api/status?token=s3cret-tok")
        await response.read()
    finally:
        await client.close()
    served = [ln for ln in sink.lines if "/api/status" in ln]
    check("the server's access log line names the path", bool(served), sink.lines[-3:])
    check("the server's access log line has no query", served and all("s3cret-tok" not in ln for ln in served), served)

    quiet = logging.getLogger("selkies-test-access-quiet")
    quiet.setLevel(logging.WARNING)
    check("an access logger below INFO reports itself disabled",
          PathOnlyAccessLogger(quiet, "").enabled is False and access.enabled is True)


async def switch_origin_cases() -> None:
    """POST /api/switch through the real middleware on a stub server."""
    switches = []

    async def run_case(settings: _Settings, headers: dict, expect: int, label: str,
                       method: str = "POST", path: str = "/api/switch") -> None:
        server = CentralizedStreamServer.__new__(CentralizedStreamServer)
        server.settings = settings

        async def switch(request):
            switches.append(request.headers.get("Origin"))
            return web.Response(text="OK")

        app = web.Application(middlewares=[server._auth_middleware])
        app["settings"] = settings
        app.router.add_post("/api/switch", switch)
        app.router.add_get("/", lambda request: web.Response(text="OK"))
        client = TestClient(TestServer(app))
        await client.start_server()
        try:
            response = await client.request(method, path, headers=headers, json={"mode": "webrtc"})
            await response.read()
            check(label, response.status == expect, f"status {response.status}, expected {expect}")
        finally:
            await client.close()

    basic = {"Authorization": _basic("user", "secret")}
    foreign = {"Origin": "https://evil.example"}
    await run_case(_Settings(), basic, 200, "switch: Basic auth, no Origin (curl) passes")
    await run_case(_Settings(), dict(basic, Origin="http://127.0.0.1"), 403,
                   "switch: Basic auth, Origin that is not the Host is refused")
    await run_case(_Settings(), dict(basic, **foreign), 403,
                   "switch: Basic auth, cross-site Origin refused")
    await run_case(_Settings(allowed_origins="https://evil.example"), dict(basic, **foreign), 200,
                   "switch: a listed Origin passes")
    await run_case(_Settings(allowed_origins="*"), dict(basic, **foreign), 200,
                   "switch: allowed_origins=* passes any Origin")
    await run_case(_Settings(enable_basic_auth=(False,)), foreign, 403,
                   "switch: open server, cross-site Origin refused")
    await run_case(_Settings(master_token="mt"), dict(foreign, Authorization="Bearer mt"), 200,
                   "switch: Bearer master token passes regardless of Origin")
    await run_case(_Settings(master_token="mt"), dict(basic, **foreign), 403,
                   "switch: master token set, Basic fallback still Origin-gated")
    await run_case(_Settings(), dict(basic, **foreign), 200,
                   "non-control route: cross-site Origin is not the switch's business",
                   method="GET", path="/")

    # Same-origin browser POSTs carry an Origin equal to the Host (with or
    # without the port a proxy strips), which is what the dashboards send.
    server = CentralizedStreamServer.__new__(CentralizedStreamServer)
    server.settings = _Settings()
    app = web.Application(middlewares=[server._auth_middleware])
    app["settings"] = server.settings
    app.router.add_post("/api/switch", lambda request: web.Response(text="OK"))
    client = TestClient(TestServer(app))
    await client.start_server()
    try:
        host = client.host
        port = client.port
        response = await client.post(
            "/api/switch", headers=dict(basic, Origin=f"http://{host}:{port}"), json={"mode": "webrtc"})
        await response.read()
        check("switch: same-origin POST (Origin == Host) passes", response.status == 200, response.status)
        response = await client.post(
            "/api/switch", headers=dict(basic, Origin=f"http://{host}:{port}", Host=host), json={"mode": "webrtc"})
        await response.read()
        check("switch: same-origin POST behind a port-stripping proxy passes",
              response.status == 200, response.status)
    finally:
        await client.close()


def mk_verdict_cases() -> None:
    """The verdict helpers, in a fresh interpreter per settings variant (the
    settings singleton reads SELKIES_* at import)."""
    base_env = {k: v for k, v in os.environ.items() if not k.startswith("SELKIES_")}
    code = r"""
import json, selkies.selkies as S
out = {}
S.user_tokens.clear()
S.user_tokens.update({"ctrl": {"role": "controller", "slot": 1}, "view": {"role": "viewer", "slot": None}})
S.active_mk_token = None
out["ctrl_no_mk"] = S._mk_access_verdict(S.user_tokens["ctrl"], token="ctrl")
out["view_no_mk"] = S._mk_access_verdict(S.user_tokens["view"], token="view")
S.active_mk_token = "view"
out["view_holds_mk"] = S._mk_access_verdict(S.user_tokens["view"], token="view")
out["ctrl_outranked"] = S._mk_access_verdict(S.user_tokens["ctrl"], token="ctrl")
out["ctrl_outranked_perms_token"] = S._mk_access_verdict({"role": "controller", "token": "ctrl"})
out["lookup_hit"] = S._lookup_session_token("view") is S.user_tokens["view"]
out["lookup_miss"] = S._lookup_session_token("vie") is None and S._lookup_session_token("") is None
out["lookup_none"] = S._lookup_session_token(None) is None
print(json.dumps(out))
"""
    with tempfile.TemporaryDirectory(prefix="selkies-guards-") as home:
        def run(**env):
            proc = subprocess.run(
                [sys.executable, "-c", code], capture_output=True, text=True, timeout=180,
                env=dict(base_env, PYTHONPATH=os.path.join(REPO, "src"),
                         SELKIES_FILE_MANAGER_PATH=home, **env))
            lines = [ln for ln in proc.stdout.splitlines() if ln.startswith("{")]
            if not lines:
                check("verdict probe ran", False, (proc.stderr or proc.stdout)[-400:])
                return {}
            return json.loads(lines[-1])

        got = run()
        check("no mk token: a controller holds input", got.get("ctrl_no_mk") is True, got)
        check("no mk token: a viewer does not", got.get("view_no_mk") is False, got)
        check("viewer holding the mk token is granted (collab on)", got.get("view_holds_mk") is True, got)
        check("controller outranked by the mk token is refused",
              got.get("ctrl_outranked") is False and got.get("ctrl_outranked_perms_token") is False, got)
        check("session token lookup finds the provisioned entry", got.get("lookup_hit") is True, got)
        check("session token lookup rejects prefixes and empties",
              got.get("lookup_miss") is True and got.get("lookup_none") is True, got)
        got = run(SELKIES_ENABLE_COLLAB="false")
        check("collab off: a viewer holding the mk token stays read-only",
              got.get("view_holds_mk") is False, got)
        check("collab off: controller verdicts unchanged",
              got.get("ctrl_no_mk") is True and got.get("ctrl_outranked") is False, got)


def main() -> bool:
    asyncio.run(access_log_cases())
    asyncio.run(switch_origin_cases())
    mk_verdict_cases()
    print(f"[control-plane] {passed}/{passed + failed} passed", flush=True)
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
