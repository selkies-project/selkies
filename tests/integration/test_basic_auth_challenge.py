#!/usr/bin/env python3
"""Every Basic-auth rejection must carry a WWW-Authenticate challenge.

Without one the browser renders the body as a page instead of re-prompting, so a
mistyped password ends the session. The success path must not carry one.
"""
import asyncio
import base64
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))

from aiohttp import web
from aiohttp.test_utils import TestClient, TestServer

from selkies.stream_server import CentralizedStreamServer

USER = "user"
PASSWORD = "right"


class _Settings:
    enable_basic_auth = (True,)
    basic_auth_user = USER
    basic_auth_password = PASSWORD
    basic_auth_viewonly_password = ""
    master_token = ""
    subfolder = ""


def _basic(user, password):
    return "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()


CASES = [
    ("no header", {}, 401),
    ("wrong password", {"Authorization": _basic(USER, "wrong")}, 401),
    ("wrong user", {"Authorization": _basic("nobody", PASSWORD)}, 401),
    ("no colon", {"Authorization": "Basic " + base64.b64encode(b"nocolon").decode()}, 401),
    ("undecodable", {"Authorization": "Basic !!!not-base64!!!"}, 401),
    ("correct", {"Authorization": _basic(USER, PASSWORD)}, 200),
]


async def _run():
    server = CentralizedStreamServer.__new__(CentralizedStreamServer)
    server.settings = _Settings()

    async def handler(request):
        return web.Response(text="OK")

    app = web.Application(middlewares=[server._auth_middleware])
    app["settings"] = server.settings
    app.router.add_get("/", handler)

    client = TestClient(TestServer(app))
    await client.start_server()
    failures = []
    try:
        for name, headers, expect_status in CASES:
            response = await client.get("/", headers=headers)
            challenge = response.headers.get("WWW-Authenticate")
            if response.status != expect_status:
                failures.append(f"{name}: status {response.status}, expected {expect_status}")
                continue
            if expect_status == 401 and not (challenge or "").startswith("Basic realm="):
                failures.append(f"{name}: 401 without a Basic challenge ({challenge!r})")
            elif expect_status == 200 and challenge is not None:
                failures.append(f"{name}: authenticated request challenged ({challenge!r})")
            else:
                print(f"PASS  {name}: {response.status} {challenge!r}")
    finally:
        await client.close()
    return failures


def main():
    failures = asyncio.run(_run())
    for failure in failures:
        print(f"FAIL  {failure}")
    print("RESULT:", "PASS" if not failures else f"{len(failures)} FAILED")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
