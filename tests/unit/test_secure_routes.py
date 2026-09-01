#!/usr/bin/env python3
"""Secure mode binds the API routes to the session token.

The real auth middleware runs on a stub application carrying the route set the
server registers. With a master token set and Basic auth off, uploads, the file
listing, TURN and metrics want a provisioned session token — as a Bearer header,
as ``?token=``, or as the cookie the web client sets — or the master token;
the control endpoints (``/api/tokens``, ``/api/switch``) take the master token
as ``Authorization: Bearer`` or, beside a Basic login's Authorization header,
in the named fallback header; the
liveness routes, the static client and the WebSocket handshakes (which carry
their own token gate) stay as they are. A viewer-role token is refused where
the view-only password is. With Basic auth on as well, a token is accepted
beside the Basic credentials; without a master token nothing changes.
"""
import asyncio
import base64
import logging
import os
import sys
import tempfile
import urllib.parse

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

# The server module reads its settings at import; keep the shell's out of it
# and point the upload directory it creates at a scratch one.
for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
_SCRATCH = tempfile.mkdtemp(prefix="selkies-secure-routes-")
os.environ["SELKIES_FILE_MANAGER_PATH"] = _SCRATCH

from aiohttp import web  # noqa: E402
from aiohttp.test_utils import TestClient, TestServer  # noqa: E402

import selkies.selkies as S  # noqa: E402
from selkies.stream_server import (  # noqa: E402
    AUTH_REALM, MASTER_TOKEN_HEADER, SESSION_TOKEN_COOKIE, CentralizedStreamServer,
)

MASTER = "unit-master-token"
CTRL = "unit-ctrl-token"
VIEW = "unit-view-token"
ODD = "unit odd/token;=ü"

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [secure-routes] {label}  {detail}", flush=True)


class _Settings:
    """The settings fields the auth middleware reads, with per-case overrides."""

    def __init__(self, **over) -> None:
        self.enable_basic_auth = (False,)
        self.basic_auth_user = "user"
        self.basic_auth_password = "secret"
        self.basic_auth_viewonly_password = ""
        self.master_token = MASTER
        self.subfolder = ""
        self.allowed_origins = ""
        for key, value in over.items():
            setattr(self, key, value)


def _basic(user: str, password: str) -> str:
    return "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()


def _bearer(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def _named(token: str) -> dict:
    return {MASTER_TOKEN_HEADER: f"Bearer {token}"}


def _cookie(token: str) -> dict:
    return {"Cookie": f"{SESSION_TOKEN_COOKIE}={urllib.parse.quote(token, safe='')}"}


class _App:
    """A stub of the server's route set behind the real middleware.

    Every handler answers 200 with the role ceiling the middleware tagged;
    the upload and switch handlers refuse a viewer ceiling through the
    server's own _viewer_ceiling, the way the real ones do.
    """

    def __init__(self, settings: _Settings) -> None:
        self.server = CentralizedStreamServer.__new__(CentralizedStreamServer)
        self.server.settings = settings
        self.settings = settings
        prefix = settings.subfolder
        app = web.Application(middlewares=[self.server._auth_middleware])
        app["settings"] = settings
        app.router.add_get(f"{prefix}/api/status", self._ok)
        app.router.add_get(f"{prefix}/api/health", self._ok)
        app.router.add_post(f"{prefix}/api/tokens", self._ok)
        app.router.add_post(f"{prefix}/api/switch", self._controller_only)
        app.router.add_post(f"{prefix}/api/upload", self._controller_only)
        app.router.add_get(f"{prefix}/api/files/{{path:.*}}", self._ok)
        app.router.add_get(f"{prefix}/api/turn", self._ok)
        app.router.add_get(f"{prefix}/api/metrics", self._ok)
        app.router.add_get(f"{prefix}/api/websockets", self._ok)
        app.router.add_get(f"{prefix}/api/webrtc/signaling", self._ok)
        app.router.add_get(f"{prefix}/", self._ok)
        app.router.add_get(f"{prefix}/index.html", self._ok)
        self.client = TestClient(TestServer(app))

    async def _ok(self, request: web.Request) -> web.Response:
        return web.Response(text=request.get("auth_role_ceiling") or "none")

    async def _controller_only(self, request: web.Request) -> web.Response:
        if self.server._viewer_ceiling(request):
            return web.Response(status=403, text="viewer")
        return web.Response(text=request.get("auth_role_ceiling") or "none")

    async def __aenter__(self) -> "_App":
        await self.client.start_server()
        return self

    async def __aexit__(self, *exc) -> None:
        await self.client.close()

    async def call(self, method: str, path: str, headers=None, host_origin: bool = False) -> tuple:
        """One request; returns (status, body, WWW-Authenticate)."""
        headers = dict(headers or {})
        if host_origin:
            headers["Origin"] = f"http://{self.client.host}:{self.client.port}"
        # No body on an Upgrade request: the server parser stops at the
        # headers there, and an unread body is re-fed as the next request.
        data = None if "Upgrade" in headers else b"x"
        response = await self.client.request(method, path, headers=headers, data=data)
        body = await response.text()
        return response.status, body, response.headers.get("WWW-Authenticate")


def _provision() -> None:
    S.user_tokens.clear()
    S.user_tokens.update({
        CTRL: {"role": "controller", "slot": 1},
        VIEW: {"role": "viewer", "slot": None},
        ODD: {"role": "controller", "slot": 2},
    })
    S.active_mk_token = None


BEARER_CHALLENGE = f'Bearer realm="{AUTH_REALM}"'
GATED = [("POST", "/api/upload"), ("GET", "/api/files/"), ("GET", "/api/files/sub/a.txt"),
         ("GET", "/api/turn"), ("GET", "/api/metrics")]


async def secure_basic_off() -> None:
    _provision()
    async with _App(_Settings()) as app:
        for method, path in GATED:
            status, _, challenge = await app.call(method, path)
            check(f"no credential: {method} {path} is refused",
                  status == 401 and challenge == BEARER_CHALLENGE, f"{status} {challenge!r}")
        for path in ("/api/status", "/api/health", "/", "/index.html"):
            status, _, _ = await app.call("GET", path)
            check(f"no credential: GET {path} stays open", status == 200, status)
        for path in ("/api/websockets", "/api/webrtc/signaling"):
            status, _, _ = await app.call(
                "GET", path, headers={"Upgrade": "websocket", "Connection": "Upgrade"})
            check(f"WebSocket upgrade to {path} passes the middleware (own token gate)",
                  status == 200, status)
            status, _, _ = await app.call("GET", path)
            check(f"plain GET {path} (the client's mode probe) without a token is refused",
                  status == 401, status)
            status, body, _ = await app.call("GET", path, headers=_bearer(CTRL))
            check(f"plain GET {path} with a token reaches the handler",
                  status == 200 and body == "controller", f"{status} {body}")
        status, _, _ = await app.call(
            "POST", "/api/upload", headers={"Upgrade": "websocket", "Connection": "Upgrade"})
        check("an Upgrade header on a non-WebSocket route does not skip the token gate",
              status == 401, status)

        for method, path in GATED:
            status, body, _ = await app.call(method, path, headers=_bearer(CTRL))
            check(f"Bearer controller token: {method} {path} accepted as controller",
                  status == 200 and body == "controller", f"{status} {body}")
            status, body, _ = await app.call(method, path, headers=_bearer(MASTER))
            check(f"Bearer master token: {method} {path} accepted as controller",
                  status == 200 and body == "controller", f"{status} {body}")
            status, body, _ = await app.call(method, path, headers=_bearer(CTRL[:-1]))
            check(f"Bearer prefix of a token: {method} {path} refused", status == 401, status)
            status, body, _ = await app.call(method, path, headers=_cookie(CTRL[:-1]))
            check(f"cookie prefix of a token: {method} {path} refused", status == 401, status)

        for path in ("/api/files/", "/api/files/sub/a.txt", "/api/turn", "/api/metrics"):
            status, body, _ = await app.call("GET", path, headers=_cookie(CTRL))
            check(f"cookie: GET {path} accepted", status == 200 and body == "controller", f"{status} {body}")
            status, body, _ = await app.call("GET", f"{path}?token={CTRL}")
            check(f"?token= query: GET {path} accepted", status == 200 and body == "controller", f"{status} {body}")
        status, body, _ = await app.call("POST", "/api/upload", headers=_cookie(CTRL))
        check("cookie: POST /api/upload without an Origin (curl) accepted", status == 200, status)
        status, body, _ = await app.call("POST", "/api/upload", headers=_cookie(CTRL), host_origin=True)
        check("cookie: same-origin POST /api/upload accepted", status == 200, status)
        status, body, _ = await app.call(
            "POST", "/api/upload", headers=dict(_cookie(CTRL), Origin="https://evil.example"))
        check("cookie: cross-site POST /api/upload refused by Origin", status == 403, status)
        status, body, _ = await app.call(
            "POST", "/api/upload", headers=dict(_bearer(CTRL), Origin="https://evil.example"))
        check("Bearer: cross-site Origin is not the header's business", status == 200, status)
        status, body, _ = await app.call("POST", f"/api/upload?token={CTRL}")
        check("?token= query: POST /api/upload accepted", status == 200, status)
        status, body, _ = await app.call("GET", "/api/files/", headers=_cookie(ODD))
        check("cookie: a URL-encoded token with reserved characters matches",
              status == 200 and body == "controller", f"{status} {body}")
        status, body, _ = await app.call(
            "GET", "/api/files/", headers={"Cookie": f"{SESSION_TOKEN_COOKIE}={CTRL}"})
        check("cookie: a raw token value matches too", status == 200, status)
        status, body, _ = await app.call(
            "GET", "/api/files/", headers=dict(_bearer(CTRL[:-1]), **_cookie(CTRL)))
        check("an invalid Bearer beside a valid cookie still authenticates", status == 200, status)

        status, body, _ = await app.call("POST", "/api/upload", headers=_bearer(VIEW))
        check("viewer token: upload refused", status == 403 and body == "viewer", f"{status} {body}")
        status, body, _ = await app.call("POST", "/api/upload", headers=_cookie(VIEW))
        check("viewer token (cookie): upload refused", status == 403, status)
        for path in ("/api/files/", "/api/turn", "/api/metrics"):
            status, body, _ = await app.call("GET", path, headers=_bearer(VIEW))
            check(f"viewer token: GET {path} accepted as viewer",
                  status == 200 and body == "viewer", f"{status} {body}")

        status, _, challenge = await app.call("POST", "/api/tokens", headers=_bearer(CTRL))
        check("session token cannot provision tokens", status == 401 and challenge == BEARER_CHALLENGE,
              f"{status} {challenge!r}")
        status, _, _ = await app.call("POST", "/api/tokens", headers=_bearer(MASTER))
        check("master token provisions tokens", status == 200, status)
        status, _, _ = await app.call("POST", "/api/tokens", headers=_named(MASTER))
        check("named-header master token provisions tokens", status == 200, status)
        status, _, _ = await app.call(
            "POST", "/api/tokens",
            headers=dict(_named(MASTER), Authorization=_basic("proxyuser", "proxypass")))
        check("named-header master token provisions beside a Basic Authorization",
              status == 200, status)
        status, _, _ = await app.call(
            "POST", "/api/tokens", headers=dict(_bearer(MASTER), **_named("junk")))
        check("Authorization Bearer is tried before the named header", status == 200, status)
        status, _, challenge = await app.call("POST", "/api/tokens", headers=_named(CTRL))
        check("named header takes only the master token", status == 401 and challenge == BEARER_CHALLENGE,
              f"{status} {challenge!r}")
        status, _, _ = await app.call("POST", "/api/upload", headers=_named(MASTER))
        check("named header is a credential only on the control endpoints", status == 401, status)
        status, _, _ = await app.call("POST", "/api/switch", headers=_named(MASTER))
        check("named-header master token switches modes", status == 200, status)
        status, _, _ = await app.call(
            "POST", "/api/switch",
            headers=dict(_named(MASTER), Authorization=_basic("proxyuser", "proxypass")))
        check("named-header master token switches modes beside a Basic Authorization",
              status == 200, status)
        status, _, challenge = await app.call("POST", "/api/switch", headers=_named(CTRL))
        check("named header on the switch takes only the master token",
              status == 401 and challenge == BEARER_CHALLENGE, f"{status} {challenge!r}")
        status, _, challenge = await app.call("POST", "/api/switch", headers=_bearer(CTRL))
        check("session token cannot switch modes (master only without Basic)",
              status == 401 and challenge == BEARER_CHALLENGE, f"{status} {challenge!r}")
        status, _, challenge = await app.call("POST", "/api/switch", headers=_cookie(CTRL))
        check("cookie cannot switch modes either", status == 401, status)
        status, _, _ = await app.call("POST", "/api/switch", headers=_bearer(MASTER))
        check("master token switches modes", status == 200, status)

    S.user_tokens.clear()
    async with _App(_Settings()) as app:
        status, _, _ = await app.call("GET", "/api/files/", headers=_bearer(CTRL))
        check("a revoked token is refused", status == 401, status)


async def secure_subfolder() -> None:
    _provision()
    async with _App(_Settings(subfolder="/desk")) as app:
        status, _, _ = await app.call("GET", "/desk/api/files/")
        check("subfolder: the listing is gated", status == 401, status)
        status, _, _ = await app.call("GET", "/desk/api/files/", headers=_bearer(CTRL))
        check("subfolder: the listing takes the token", status == 200, status)
        status, _, _ = await app.call("GET", "/desk/api/status")
        check("subfolder: liveness stays open", status == 200, status)
        status, _, _ = await app.call("GET", "/desk/")
        check("subfolder: the static client stays open", status == 200, status)


async def secure_basic_on() -> None:
    _provision()
    async with _App(_Settings(enable_basic_auth=(True,))) as app:
        status, _, challenge = await app.call("POST", "/api/upload")
        check("Basic+token: no credential gets the Basic challenge",
              status == 401 and (challenge or "").startswith("Basic realm="), f"{status} {challenge!r}")
        status, _, challenge = await app.call("GET", "/")
        check("Basic+token: the page itself wants Basic", status == 401 and (challenge or "").startswith("Basic"),
              f"{status} {challenge!r}")
        status, body, _ = await app.call("POST", "/api/upload", headers={"Authorization": _basic("user", "secret")})
        check("Basic+token: Basic credentials upload", status == 200 and body == "controller", f"{status} {body}")
        status, body, _ = await app.call("POST", "/api/upload", headers=_bearer(CTRL))
        check("Basic+token: a session token uploads beside Basic (a script's header replaces the browser's)",
              status == 200 and body == "controller", f"{status} {body}")
        status, body, _ = await app.call("GET", "/api/files/", headers=_cookie(CTRL))
        check("Basic+token: the cookie lists files", status == 200, status)
        status, body, _ = await app.call("POST", "/api/upload", headers=_bearer(VIEW))
        check("Basic+token: a viewer token cannot upload", status == 403, status)
        status, _, challenge = await app.call("POST", "/api/upload", headers=_bearer(CTRL[:-1]))
        check("Basic+token: an invalid token falls through to the Basic challenge",
              status == 401 and (challenge or "").startswith("Basic"), f"{status} {challenge!r}")
        status, _, challenge = await app.call("POST", "/api/switch", headers=_bearer(CTRL))
        check("Basic+token: a session token does not switch modes",
              status == 401 and (challenge or "").startswith("Basic"), f"{status} {challenge!r}")
        status, _, _ = await app.call("POST", "/api/switch", headers={"Authorization": _basic("user", "secret")})
        check("Basic+token: Basic credentials switch modes as before", status == 200, status)
        status, _, _ = await app.call(
            "GET", "/api/websockets", headers={"Upgrade": "websocket", "Connection": "Upgrade"})
        check("Basic+token: WebSocket upgrades skip Basic (own token gate)", status == 200, status)
        status, _, challenge = await app.call(
            "GET", "/api/files/", headers={"Upgrade": "websocket", "Connection": "Upgrade"})
        check("Basic+token: an Upgrade header on another route does not skip Basic",
              status == 401 and (challenge or "").startswith("Basic"), f"{status} {challenge!r}")


async def legacy_modes() -> None:
    _provision()
    async with _App(_Settings(master_token="")) as app:
        for method, path in GATED + [("POST", "/api/switch"), ("GET", "/")]:
            status, body, _ = await app.call(method, path, headers=dict(_bearer("junk"), **_cookie("junk")))
            check(f"no master token, Basic off: {method} {path} open as before",
                  status == 200 and body == "none", f"{status} {body}")
    async with _App(_Settings(master_token="", enable_basic_auth=(True,),
                              basic_auth_viewonly_password="look")) as app:
        status, _, challenge = await app.call("POST", "/api/upload", headers=_bearer(CTRL))
        check("no master token, Basic on: a Bearer token is not a credential",
              status == 401 and (challenge or "").startswith("Basic"), f"{status} {challenge!r}")
        status, _, _ = await app.call("POST", "/api/upload", headers=_cookie(CTRL))
        check("no master token, Basic on: the cookie is not a credential", status == 401, status)
        status, body, _ = await app.call("POST", "/api/upload", headers={"Authorization": _basic("user", "secret")})
        check("no master token, Basic on: Basic uploads", status == 200 and body == "controller", f"{status} {body}")
        status, body, _ = await app.call("POST", "/api/upload", headers={"Authorization": _basic("user", "look")})
        check("no master token, Basic on: the view-only password cannot upload",
              status == 403 and body == "viewer", f"{status} {body}")
        status, body, _ = await app.call("GET", "/api/files/", headers={"Authorization": _basic("user", "look")})
        check("no master token, Basic on: the view-only password lists files",
              status == 200 and body == "viewer", f"{status} {body}")


def main() -> bool:
    # The server module configures logging at import; the stub server's access
    # lines and the middleware's refusal warnings are not the output here.
    logging.getLogger("aiohttp.access").setLevel(logging.ERROR)
    logging.getLogger("stream_server").setLevel(logging.ERROR)
    asyncio.run(secure_basic_off())
    asyncio.run(secure_subfolder())
    asyncio.run(secure_basic_on())
    asyncio.run(legacy_modes())
    print(f"[secure-routes] {passed}/{passed + failed} passed", flush=True)
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
